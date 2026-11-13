#!/usr/bin/env node
'use strict';
// Recorded rest of a turn's model routing for crates/chat-prepare (slice P5c): the contract src/reroute.rs is replayed
// against (with src/roles.rs before it and src/offer.rs for the tools).
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-prepare/tests/record_reroute.cjs > crates/chat-prepare/tests/fixtures/node-reroute.v1.json
//   ... node crates/chat-prepare/tests/record_reroute.cjs --check crates/chat-prepare/tests/fixtures/node-reroute.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// The checkout needs server/wasm/dav-parse.wasm (the long-context pick is long-profile in it; selectedToolboxIds and the
// egress rules too); nothing from node_modules, no network. Every expectation comes from noevia-core's own code; the
// inputs are synthetic and QA-named (hand-made edges plus a seeded PRNG), passed through JSON first so Node reads exactly
// what is recorded:
//   turns  chat.cjs createChatHandler().handleChat, whole turns on the local default provider or a QA cloud provider,
//          no Inference machines, no routing modes, no native executor, chat framing off. The toolboxes are a real
//          createToolboxes (core box, Drive box not connected, QA MCP boxes) over a prefill stub measuring per model.
//          The seams are stubs that answer per case and log their calls in order, up to the meta event:
//          servedCatalogue (an answer per call), modelsInstalled, lastLoadedModel, classifyFastOrSmart, the tool gate
//          (toolGate.evaluate, the tools it is shown), the template tools gate (templateToolsGate.allowsTools: the move's
//          question per candidate model) and the prefill measurement (budgetFor, which resolveTools asks per model).
//          The long-context profile is core's own createLongProfiles over the wasm. A template check after the meta
//          event (the request's own, outside this slice) answers true, so the reply request carries the offered tools.
//          Recorded: those calls, the refusal (json), the meta event's model, route, routingDecision, contextProfile
//          and sampling, the status line after it, the tools_scope event, the `[tools] ... cannot take tools` and
//          `[long-profile]` lines, and the reply request's model and tools (names and the sha256 of their JSON); or
//          what handleChat threw.
// Each source file's sha256 is recorded (and chat.cjs's ported blocks), so a changed Node file needs a fresh recording.
// Node's console output goes to stderr; stdout is the fixture.

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
const READ_FILES = ['server/chat.cjs', 'server/index.cjs', 'server/dav-parse.lock', 'server/wasm/dav-parse.wasm'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex');
function sourcesRead() {
  const server = `${path.join(root, 'server')}${path.sep}`;
  const loaded = Object.keys(require.cache).filter((k) => k.startsWith(server)).map((k) => path.relative(root, k).split(path.sep).join('/'));
  if (loaded.some((k) => k.includes('node_modules/'))) throw new Error('a module from node_modules was loaded: the header says nothing is');
  return Object.fromEntries([...new Set([...READ_FILES, ...loaded])].sort().map((f) => [f, sha(f)]));
}
const serverRequire = (f) => require(path.join(root, 'server', f));
// index.cjs is not loaded (it starts the server); the wiring the port follows is checked to be there.
const indexText = fs.readFileSync(path.join(root, 'server/index.cjs'), 'utf8');
for (const wiring of ["  modelManager, templateToolsGate: require('./chat-template-caps.cjs').toolsGate,",
  '  autoRoles, lastLoadedModel, classifyFastOrSmart, servedCatalogue, modelsInstalled, missingRoles, staleRolesError,',
  '  DEFAULT_TOOLBOXES, CONNECTOR_BOXES, connectedBoxes, allToolboxes, resolveTools, isWriteTool, executeToolCall,']) {
  if (!indexText.includes(wiring)) throw new Error(`index.cjs no longer holds ${JSON.stringify(wiring.slice(0, 60))}`);
}
// index.cjs leaves longProfiles to chat.cjs's default, core's createLongProfiles() over the wasm.
if (indexText.includes('longProfiles')) throw new Error('index.cjs now wires longProfiles itself');
const toolboxesLib = serverRequire('toolboxes.cjs');
const { createDriveTools } = serverRequire('gdrive-tools.cjs');
const pageSave = serverRequire('specialist-page-save.cjs');
const toolModes = serverRequire('tools-mode.cjs');
const { missingRoles, staleRolesError } = serverRequire('auto-roles-check.cjs');
const diaryExtras = serverRequire('diary-extras.cjs');
const { createChatHandler } = serverRequire('chat.cjs');

// ── chat.cjs's ported lines ─────────────────────────────────────────────────────────────────
const chatText = fs.readFileSync(path.join(root, 'server/chat.cjs'), 'utf8');
const blocks = {};
function cut(name, start, end) {
  const once = (needle) => {
    const at = chatText.indexOf(needle);
    if (at < 0 || chatText.indexOf(needle, at + 1) >= 0) throw new Error(`chat.cjs holds ${JSON.stringify(needle.slice(0, 60))} ${at < 0 ? 'nowhere' : 'twice'}`);
    return at;
  };
  const from = once(start), to = once(end) + end.length;
  blocks[name] = { lines: [chatText.slice(0, from).split('\n').length, chatText.slice(0, to).split('\n').length - 1],
    sha256: crypto.createHash('sha256').update(chatText.slice(from, to)).digest('hex') };
}
cut('toolMove', '    const routedOnMachine = !!(provider.machine && machineTarget && wantsAuto && machineTarget.role === routedRole);\n',
  '        resolved = resolveFor(model); resolvedFor = model;\n      }\n    }\n');
cut('longProfile', '    let contextSwitch = false, contextProfile = null;\n', '    if (model !== resolvedFor) { resolved = resolveFor(model); resolvedFor = model; }\n');
cut('metaProfile', 'contextProfile: contextProfile || undefined,', 'contextProfile: contextProfile || undefined,');
cut('status', '    if (contextSwitch && !readingImages) send(', "'Reading image sources — model loading and visual processing may take a moment…' : 'Preparing response…' });\n");

// ── Deterministic inputs ────────────────────────────────────────────────────────────────────
let seed = 0x2026_1113 ^ 0x5c;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const pick = (list) => list[Math.floor(rand() * list.length)];
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

// QA MCP boxes (the shapes mcp-wiring.cjs hands createToolboxes) and one createToolboxes over them.
const fn = (name, description, props = {}) => ({ type: 'function', function: { name, description, parameters: { type: 'object', properties: props } } });
const q = { query: { type: 'string', description: 'What to look for (QA)' } };
const box = (id, label, server, tools, reads) => ({ id, label, description: `${label} for QA`, source: 'mcp', server, tools, reads });
const MCP = viaJson([
  box('web-search', 'Web search', 'qa-web', [fn('web_search', 'Search the web (QA).', q), fn('web_news', 'Recent news (QA).', q)], ['web_search', 'web_news']),
  box('nextcloud-notes', 'Notes', 'nextcloud', [fn('nc_notes_search_notes', 'Search QA notes.', q), fn('nc_notes_create_note', 'Create a QA note.', { title: { type: 'string' } })],
    ['nc_notes_search_notes']),
  box('qa-many', 'QA many', 'qa-many', Array.from({ length: 30 }, (_, k) => fn(`qa_many_${k}`, `QA tool ${k}. ${'QA '.repeat(30 + 9 * k)}`)),
    Array.from({ length: 30 }, (_, k) => `qa_many_${k}`)),
]);
const SERVER_WRITES = ['nc_notes_create_note'];
let current = null; // the running turn: { spec, seam }
const driveTools = createDriveTools({ accounts: { forUser: () => ({ drive: { state: () => ({ state: 'none' }) } }) }, cap: 8000 });
const TB = toolboxesLib.createToolboxes({
  boxes: [driveTools.box], kiwixTools: null, driveTools,
  specialistTools: pageSave.createPageSave({ store: {} }),
  mcpBoxes: () => MCP,
  mcpTools: () => new Map(MCP.flatMap((b) => b.tools.map((t) => [t.function.name, { readOnly: !SERVER_WRITES.includes(t.function.name), serverId: b.server }]))),
  offered: () => true,
  prefill: {
    budgetFor: (model, target, scope) => {
      current.seam(['budgetFor', model, target, ...(scope === undefined ? [] : [scope])]);
      const m = (current.spec.measured || {})[model];
      return m === undefined ? null : m;
    },
    rateFor: () => 0.5,
  },
});
const boxRecord = (b) => ({ id: b.id, label: b.label, source: b.source, inApp: b.inApp, server: b.server, tools: b.tools, reads: b.reads });

const PROVIDER = 'http://qa-default.invalid';
const PROVIDERS = {
  default: { id: 'default', label: 'QA local', baseUrl: PROVIDER },
  'qa-cloud': { id: 'qa-cloud', label: 'QA Cloud', baseUrl: `${PROVIDER}/qa-cloud`, external: true, shared: true },
};
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-chat-reroute-'));
process.on('exit', () => fs.rmSync(tmp, { recursive: true, force: true }));

// Every model name a case may answer with, so the engine reports it loaded with room (no load after the meta event).
function modelsOf(spec) {
  const names = new Set();
  const add = (v) => { if (typeof v === 'string' && v) { names.add(v); names.add(`${v}-long`); } };
  for (const v of Object.values(spec.roles || {})) add(v);
  add(spec.project?.model); add(spec.loaded);
  for (const c of spec.catalogues) for (const r of Array.isArray(c) ? c : []) { add(r?.name); add(r?.longVariant); }
  return [...names];
}

function handlerFor(dir, spec, log) {
  const fetch = async (url, init) => {
    if (!String(url).startsWith(PROVIDER)) throw new Error('QA: no network');
    log.requests.push(JSON.parse(init.body));
    return { ok: true, body: (async function* () { yield Buffer.from(`data: ${JSON.stringify({ choices: [{ delta: { content: 'QA ok' } }] })}\n\n`); })() };
  };
  let catalogueCall = 0;
  const seam = log.seam;
  const loadedModels = modelsOf(spec);
  return createChatHandler({
    modelManager: { enabled: spec.managerEnabled !== false,
      health: async () => ({ ok: true, body: { all_models_loaded: loadedModels.map((m) => ({ model_name: m, loaded: true, recipe_options: { ctx_size: 1048576 } })) } }),
      ...(spec.props === false ? {} : { props: async () => ({ ok: true, body: {} }) }) },
    templateToolsGate: {
      allowsTools: async (manager, model, signal, scope) => {
        seam(['allowsTools', model, ...(scope === undefined ? [] : [scope])]);
        if (log.meta) return true;
        const v = (spec.toolsOk || {})[model];
        return v === undefined ? true : v;
      },
      markUnsupported() {},
    },
    toolGate: spec.gate === null ? null : {
      evaluate: async (message, tools, ...rest) => { seam(['gate', Array.isArray(tools) ? tools.map((t) => t?.function?.name) : null, rest.length]); return structuredClone(spec.gate); },
      record() {},
    },
    reasoningEffort: serverRequire('reasoning-effort.cjs'), authService: { audit() {}, diaryEnabled: () => true }, crypto, path, fs, fetch,
    HISTORY_CAP: 1000, DEFAULT_PROVIDER_ID: 'default', createToolExchange: () => async () => 'unused',
    currentWorkspace: () => ({ userId: 'qa-user', dir, assetDir: () => '/qa-only' }),
    getProject: (id) => (spec.project && id === spec.project.id ? structuredClone(spec.project) : null), skillsIndexFor: () => [],
    getProvider: (id) => structuredClone(PROVIDERS[id]), providerHeaders: () => ({}),
    autoRoles: () => structuredClone(spec.roles ?? null), visionDescriptions: new Map(), visionProbe: async () => ({ supported: false, reason: 'qa' }),
    chatSkillRouter: { select: async () => ({ loaded: [] }) }, oauthServerIds: () => new Set(), accountReady: () => true,
    chatToolRouter: { select: async (ids) => ({ ids, routed: false }) },
    DEFAULT_TOOLBOXES: TB.DEFAULT_TOOLBOXES, CONNECTOR_BOXES: TB.CONNECTOR_BOXES, connectedBoxes: TB.connectedBoxes, allToolboxes: TB.allToolboxes,
    resolveTools: TB.resolveTools, isWriteTool: TB.isWriteTool,
    toolPolicy: { mode: () => 'allow' }, requestScope: { getStore: () => ({}) },
    rag: { filesContext: async () => null }, prefill: { recordSample() {} }, reduceToolResult: () => ({ text: 'reduced' }), diaryExtras,
    DIARY_BASE: 'http://qa-diary.invalid', TOOL_RESULT_CAP: 8000,
    json: (res, status, body) => { log.refused = { status, body }; },
    saveChats() {}, endpointApproved: () => true, diaryHeaders: () => ({}),
    lastLoadedModel: () => { seam(['lastLoadedModel']); return spec.loaded ?? null; },
    modelsInstalled: async () => { seam(['modelsInstalled']); return structuredClone(spec.catalogues[0]); },
    servedCatalogue: async () => {
      seam(['servedCatalogue']);
      const k = Math.min(catalogueCall, spec.catalogues.length - 1);
      catalogueCall += 1;
      return structuredClone(spec.catalogues[k]);
    },
    classifyFastOrSmart: async () => { seam(['classify']); return structuredClone(spec.classification); },
    missingRoles, staleRolesError,
    chatWideApproved: () => false, recordUsage() {}, recordToolUse() {}, executeToolCall: async () => 'unused', awaitApproval: async () => 'deny',
    codeTasksFor: () => [], sourceJobs: { settle: async () => ({ notReady: [], aborted: false }) },
    skillHistory: { scrub: ({ messages }) => ({ messages, removed: [] }), record() {} },
    chatFramingEnabled: () => false,
  });
}

const turns = [];
async function addTurn(name, raw) {
  const spec = viaJson({ message: 'QA question', gate: null, catalogues: [[]], ...raw });
  const dir = fs.mkdtempSync(path.join(tmp, 'qa-'));
  process.env.NOEVIA_CONTRACT_FIXED_NOW = '2026-11-13T12:00:00Z';
  process.env.TZ = 'UTC';
  const log = { requests: [], calls: [], meta: false, refused: null };
  log.seam = (entry) => { if (!log.meta) log.calls.push(entry); };
  current = { spec, seam: log.seam };
  const rawEvents = [];
  const res = new EventEmitter();
  res.writeHead = () => {}; res.setHeader = () => {};
  res.write = (line) => { const s = String(line); rawEvents.push(s); if (s.includes('"type":"meta"')) log.meta = true; return true; };
  res.end = () => { res.writableEnded = true; res.emit('finish'); };
  const { handleChat } = handlerFor(dir, spec, log);
  const body = { message: spec.message, ...(spec.project && !spec.free ? { projectId: spec.project.id } : {}), ...(spec.body || {}) };
  const from = logged.length;
  let thrown = null;
  try { await handleChat({ headers: {} }, res, body, { user: { id: 'qa-user' } }); } catch (e) { thrown = e.constructor.name; process.stderr.write(`QA ${name} threw: ${e.stack}\n`); }
  await new Promise((resolve) => setImmediate(resolve));
  const events = rawEvents.join('').split('\n\n').filter((x) => x.startsWith('data: ')).map((x) => JSON.parse(x.slice(6)));
  const metaAt = events.findIndex((e) => e.type === 'meta');
  const meta = metaAt >= 0 ? events[metaAt] : null;
  const status = metaAt >= 0 ? events.slice(metaAt).find((e) => e.type === 'status' && ['loadingLongContext', 'preparing', 'readingImages'].includes(e.id)) || null : null;
  const scope = events.find((e) => e.type === 'tools_scope') || null;
  const lines = logged.slice(from).filter((l) => / cannot take tools; /.test(l) || l.startsWith('[long-profile]'));
  const reply = log.requests.find((r) => r.stream === true) || null;
  const toolsSha256 = (tools) => crypto.createHash('sha256').update(JSON.stringify(tools)).digest('hex');
  if (!thrown && !meta && !log.refused) throw new Error(`${name}: no meta event and no refusal; events: ${rawEvents.join('').slice(0, 600)}`);
  turns.push({ name, spec, body, calls: log.calls, ...(thrown ? { throws: thrown } : {}), refused: log.refused,
    meta: meta ? { model: meta.model, route: meta.route ?? null, routingDecision: meta.routingDecision ?? null, contextProfile: meta.contextProfile ?? null,
      sampling: meta.sampling ?? null } : null,
    status: status ? { id: status.id, text: status.text } : null, scope, lines,
    reply: reply ? { model: reply.model, tools: Array.isArray(reply.tools) ? reply.tools.map((t) => t.function.name) : null,
      toolsSha256: Array.isArray(reply.tools) ? toolsSha256(reply.tools) : null } : null });
}

// Models: the size in the name sets the tool cap (12 tools at 12B or less), the measured prefill the token budget.
const FAST = 'qa-fast-4b', SMART = 'qa-smart-27b', CODE = 'qa-code-14b', CREATIVE = 'qa-creative-9b', LOADED = 'qa-loaded-12b', ALT = 'qa-alt-30b';
const ROLES = { fast: FAST, smart: SMART, code: CODE };
const row = (name, extra = {}) => ({ name, labels: [], loaded: false, ...extra });
const longRow = (name, extra = {}) => row(name, { longVariant: `${name}-long`, longLoaded: false, ...extra });
const CAT = [row(FAST), row(SMART), row(CODE), row(CREATIVE), row(LOADED), row(ALT)];
const LONG_CAT = [longRow(FAST), longRow(SMART), longRow(CODE), row(CREATIVE), longRow(LOADED), row(ALT)];
const NEEDS = { decision: { tool: 'web_search', mode: 'require', reason: 'QA web' } };
const NONE = { decision: 'none' };
const WEB = 'Search the web for the latest QA release notes';
const AUTO = (extra = {}) => ({ id: 'qa-project-1', name: 'QA Project', routing: 'auto', ...extra });
const MANUAL = (model, extra = {}) => ({ id: 'qa-project-1', name: 'QA Project', routing: 'manual', model, ...extra });
const decision = (role) => ({ role, routingDecision: { status: 'accepted', role, confidence: 0.9, reason: 'QA decision' } });

(async () => {
  // ── #1550: Automatic's move to a tool-capable role ──────────────────────────────────────
  await addTurn('move_fast_to_smart', { project: AUTO(), roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false }, catalogues: [CAT], message: WEB });
  await addTurn('move_fast_to_code', { project: AUTO(), roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false, [SMART]: false }, catalogues: [CAT], message: WEB });
  await addTurn('move_smart_to_fast', { project: AUTO(), roles: ROLES, classification: decision('smart'), gate: NEEDS, toolsOk: { [SMART]: false, [CODE]: false }, catalogues: [CAT], message: WEB });
  await addTurn('move_code_to_smart', { project: AUTO(), roles: ROLES, classification: 'code', gate: NEEDS, toolsOk: { [CODE]: false }, catalogues: [CAT], message: WEB });
  await addTurn('move_none_possible', { project: AUTO(), roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false, [SMART]: false, [CODE]: false }, catalogues: [CAT], message: WEB });
  await addTurn('move_same_model_skipped', { project: AUTO(), roles: { fast: FAST, smart: SMART, code: FAST }, classification: decision('fast'), gate: NEEDS,
    toolsOk: { [FAST]: false, [SMART]: false }, catalogues: [CAT], message: WEB });
  await addTurn('move_same_model_as_smart', { project: AUTO(), roles: { fast: FAST, smart: FAST, code: CODE }, classification: decision('fast'), gate: NEEDS,
    toolsOk: { [FAST]: false }, catalogues: [CAT], message: WEB });
  await addTurn('move_roles_without_code', { project: AUTO(), roles: { fast: FAST, smart: SMART }, classification: decision('smart'), gate: NEEDS, toolsOk: { [SMART]: false }, catalogues: [CAT], message: WEB });
  await addTurn('move_roles_fast_only', { project: AUTO(), roles: { fast: FAST }, classification: decision('smart'), gate: NEEDS, toolsOk: { [FAST]: false }, catalogues: [CAT], message: WEB });
  await addTurn('routed_takes_tools', { project: AUTO(), roles: ROLES, classification: decision('fast'), gate: NEEDS, catalogues: [CAT], message: WEB });
  await addTurn('gate_none', { project: AUTO(), roles: ROLES, classification: decision('fast'), gate: NONE, toolsOk: { [FAST]: false }, catalogues: [CAT], message: WEB });
  await addTurn('gate_absent', { project: AUTO(), roles: ROLES, classification: decision('fast'), gate: null, toolsOk: { [FAST]: false }, catalogues: [CAT], message: WEB });
  // A gate answer without a decision is left out: chat.cjs's enforcement after the meta event reads `gate.decision.tool`
  // of it and throws (outside this slice); tool-gate.cjs always answers a decision.
  await addTurn('gate_decision_text', { project: AUTO(), roles: ROLES, classification: decision('fast'), gate: { decision: 'web_search' }, toolsOk: { [FAST]: false }, catalogues: [CAT], message: WEB });
  await addTurn('tools_manual_never_moves', { project: AUTO({ toolsMode: 'manual', toolboxes: ['core', 'qa-many'] }), roles: ROLES, classification: decision('fast'), gate: NEEDS,
    toolsOk: { [FAST]: false }, catalogues: [CAT], message: WEB });
  await addTurn('tools_list_means_manual', { project: AUTO({ toolboxes: ['core'] }), roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false }, catalogues: [CAT], message: WEB });
  await addTurn('no_template_gate', { project: AUTO(), roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false }, props: false, catalogues: [CAT], message: WEB });
  await addTurn('compact_never_moves', { project: AUTO(), roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false }, catalogues: [CAT], message: '',
    body: { compactOnly: true, chatId: 'qa-compact-chat', history: [{ role: 'user', content: 'QA q1' }, { role: 'assistant', content: 'QA a1' }] } });
  await addTurn('free_chat_moves', { roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false }, catalogues: [CAT], message: WEB });
  await addTurn('manual_never_moves', { project: MANUAL(FAST), roles: ROLES, gate: NEEDS, toolsOk: { [FAST]: false }, catalogues: [CAT], message: WEB });
  await addTurn('manual_loaded_never_moves', { project: { id: 'qa-project-1', routing: 'manual' }, roles: ROLES, loaded: LOADED, gate: NEEDS, toolsOk: { [LOADED]: false }, catalogues: [CAT], message: WEB });
  await addTurn('creative_moves_to_smart', { project: AUTO(), roles: { ...ROLES, creative: CREATIVE }, classification: decision('fast'), gate: NEEDS, toolsOk: { [CREATIVE]: false },
    catalogues: [[row(FAST), row(SMART), row(CODE), row(CREATIVE, { loaded: true })]], message: 'Write a poem about QA and search the web for rhymes' });
  await addTurn('decision_as_text_moves', { project: AUTO(), roles: ROLES, classification: 'fast', gate: NEEDS, toolsOk: { [FAST]: false }, catalogues: [CAT], message: WEB });
  await addTurn('move_budget_differs', { project: AUTO(), roles: { fast: FAST, smart: ALT, code: CODE }, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false },
    measured: { [FAST]: 3000, [ALT]: 1500 }, catalogues: [CAT], message: WEB });
  // The turn's own boxes (30 QA tools): the tool cap and budget of the new model decide what is offered after the move.
  await addTurn('move_cap_differs', { project: AUTO(), roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false }, catalogues: [CAT], message: WEB,
    body: { turnToolboxes: ['qa-many'] } });
  await addTurn('move_cap_differs_to_fast', { project: AUTO(), roles: ROLES, classification: decision('smart'), gate: NEEDS, toolsOk: { [SMART]: false, [CODE]: false }, catalogues: [CAT],
    message: WEB, body: { turnToolboxes: ['qa-many', 'nextcloud-notes'] } });
  await addTurn('move_measured_budget', { project: AUTO(), roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false }, catalogues: [CAT], message: WEB,
    measured: { [FAST]: 99999, [SMART]: 1500 }, body: { turnToolboxes: ['qa-many'] } });
  await addTurn('move_chit_chat', { project: AUTO(), roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false }, catalogues: [CAT], message: 'hi' });
  await addTurn('move_from_cloud_provider_never', { project: AUTO({ provider: 'qa-cloud', model: 'qa-cloud-model' }), roles: ROLES, gate: NEEDS, toolsOk: { 'qa-cloud-model': false },
    catalogues: [CAT], message: WEB });

  // ── #1079: the long-context profile ───────────────────────────────────────────────────────
  await addTurn('high_switch', { project: MANUAL(SMART, { contextProfile: 'high' }), roles: ROLES, catalogues: [LONG_CAT] });
  await addTurn('high_long_loaded', { project: MANUAL(SMART, { contextProfile: 'high' }), roles: ROLES, catalogues: [[longRow(SMART, { longLoaded: true })]] });
  await addTurn('high_long_loaded_text', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [[longRow(SMART, { longLoaded: 'true' })]] });
  await addTurn('high_no_long', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [CAT] });
  await addTurn('high_already_long', { project: MANUAL(`${SMART}-long`, { contextProfile: 'high' }), catalogues: [[longRow(SMART), row(`${SMART}-long`)]] });
  await addTurn('high_catalogue_unreadable', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [CAT, null] });
  await addTurn('high_catalogue_not_list', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [CAT, { 0: longRow(SMART) }] });
  await addTurn('high_rows_change', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [CAT, [longRow(SMART, { longLoaded: true })]] });
  await addTurn('low_default', { project: MANUAL(SMART), catalogues: [LONG_CAT] });
  await addTurn('profile_low', { project: MANUAL(SMART, { contextProfile: 'low' }), catalogues: [LONG_CAT] });
  for (const p of ['HIGH', ' high', true, 1, ['high'], { high: true }, null]) {
    await addTurn(`profile_${JSON.stringify(p)}`, { project: MANUAL(SMART, { contextProfile: p }), catalogues: [LONG_CAT] });
  }
  await addTurn('high_loaded_model', { project: { id: 'qa-project-1', routing: 'manual', contextProfile: 'high' }, loaded: LOADED, catalogues: [LONG_CAT] });
  await addTurn('high_manager_off', { project: MANUAL(SMART, { contextProfile: 'high' }), managerEnabled: false, catalogues: [LONG_CAT] });
  await addTurn('high_cloud_provider', { project: MANUAL('qa-cloud-model', { provider: 'qa-cloud', contextProfile: 'high' }), catalogues: [[longRow('qa-cloud-model')]] });
  await addTurn('high_lemonade_is_default', { project: MANUAL(SMART, { provider: 'lemonade', contextProfile: 'high' }), catalogues: [LONG_CAT] });
  await addTurn('high_compact', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [LONG_CAT], message: '',
    body: { compactOnly: true, chatId: 'qa-compact-high', history: [{ role: 'user', content: 'QA q1' }, { role: 'assistant', content: 'QA a1' }] } });
  await addTurn('high_automatic', { project: AUTO({ contextProfile: 'high' }), roles: ROLES, classification: decision('smart'), catalogues: [LONG_CAT] });
  await addTurn('high_automatic_no_long', { project: AUTO({ contextProfile: 'high' }), roles: ROLES, classification: decision('fast'), catalogues: [CAT] });
  // Pairs the long-profile module refuses as a whole (the pick is then 'unavailable': the model, Low).
  await addTurn('pairs_duplicate_base', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [[longRow(SMART), longRow(SMART, { longLoaded: true })]] });
  await addTurn('pairs_mismatched_long', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [[row(SMART, { longVariant: 'qa-other-long' })]] });
  await addTurn('pairs_long_not_an_id', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [[row(SMART), row('qa other', { longVariant: 'qa other-long' })]] });
  await addTurn('pairs_base_number', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [[longRow(SMART), { name: 5, longVariant: '5-long' }]] });
  await addTurn('pairs_base_missing', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [[longRow(SMART), { longVariant: 'qa-x-long' }]] });
  await addTurn('pairs_long_not_text', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [[longRow(SMART), row(FAST, { longVariant: 5 }), row(CODE, { longVariant: null })]] });
  await addTurn('pairs_long_empty_base', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [[longRow(SMART), row('', { longVariant: '-long' })]] });
  await addTurn('pairs_long_long', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [[longRow(SMART), row(`${FAST}-long`, { longVariant: `${FAST}-long-long` })]] });
  await addTurn('pairs_id_too_long', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [[longRow(SMART), longRow(`qa-${'x'.repeat(200)}`)]] });
  await addTurn('pairs_primitives_skipped', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [CAT, ['QA text row', 7, true, [], longRow(SMART)]] });
  await addTurn('pairs_null_row_throws', { project: { id: 'qa-project-1', routing: 'manual', contextProfile: 'high' }, loaded: SMART, catalogues: [[longRow(SMART), null]] });
  await addTurn('pairs_first_row_wins', { project: MANUAL(SMART, { contextProfile: 'high' }), catalogues: [[row(SMART, { longLoaded: true }), longRow(SMART, { longLoaded: true })]] });
  await addTurn('model_lone_surrogate', { project: MANUAL('qa-\ud83d-27b', { contextProfile: 'high' }), catalogues: [[longRow('qa-\ud83d-27b'), longRow(SMART)]] });
  await addTurn('model_astral', { project: MANUAL('qa-😀-27b', { contextProfile: 'high' }), catalogues: [[longRow('qa-😀-27b')]] });
  await addTurn('model_too_long', { project: MANUAL(`qa-${'m'.repeat(520)}`, { contextProfile: 'high' }), catalogues: [[longRow(SMART)]] });
  await addTurn('model_unicode_long_id', { project: MANUAL('qa-é-27b', { contextProfile: 'high' }), catalogues: [[longRow('qa-é-27b')]] });

  // ── Both in one turn ────────────────────────────────────────────────────────────────────
  await addTurn('move_then_long', { project: AUTO({ contextProfile: 'high' }), roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false },
    measured: { [`${SMART}-long`]: 1500 }, catalogues: [LONG_CAT], message: WEB });
  await addTurn('move_then_long_tools', { project: AUTO({ contextProfile: 'high' }), roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false },
    measured: { [SMART]: 99999, [`${SMART}-long`]: 1500 }, catalogues: [LONG_CAT], message: WEB, body: { turnToolboxes: ['qa-many'] } });
  await addTurn('high_tools_follow_long', { project: MANUAL(FAST, { contextProfile: 'high' }), measured: { [`${FAST}-long`]: 1500 }, catalogues: [LONG_CAT],
    body: { turnToolboxes: ['qa-many'] } });
  await addTurn('move_then_long_loaded', { project: AUTO({ contextProfile: 'high' }), roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false },
    catalogues: [[longRow(FAST), longRow(SMART, { longLoaded: true }), row(CODE)]], message: WEB });
  await addTurn('move_then_no_long', { project: AUTO({ contextProfile: 'high' }), roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false },
    catalogues: [[longRow(FAST), row(SMART), row(CODE)]], message: WEB });
  await addTurn('no_move_long', { project: AUTO({ contextProfile: 'high' }), roles: ROLES, classification: decision('fast'), gate: NEEDS, toolsOk: { [FAST]: false, [SMART]: false, [CODE]: false },
    catalogues: [LONG_CAT], message: WEB });

  // ── Random turns ──────────────────────────────────────────────────────────────────────────
  const POOL = [FAST, SMART, CODE, CREATIVE, LOADED, ALT];
  const MESSAGES = [WEB, 'hi', 'QA question', 'Write a poem about QA', 'Search the web for the latest QA prices and write a story', 'Fix this QA code: function f() {}',
    'Look up the current QA version online'];
  for (let n = 0; n < 160; n += 1) {
    const roles = {};
    for (const r of ['fast', 'smart', 'code', 'creative']) if (rand() < (r === 'creative' ? 0.3 : 0.75)) roles[r] = pick(POOL);
    if (!roles.fast && !roles.smart) roles[pick(['fast', 'smart'])] = pick(POOL);
    const auto = rand() < 0.75;
    const project = rand() < 0.08 ? undefined : auto ? AUTO() : MANUAL(rand() < 0.85 ? pick(POOL) : undefined);
    if (project && project.model === undefined) delete project.model;
    if (project && rand() < 0.5) project.contextProfile = rand() < 0.85 ? 'high' : pick(['low', 'HIGH', null]);
    if (project && rand() < 0.15) project.toolsMode = pick(['manual', 'auto']);
    if (project && rand() < 0.1) project.provider = 'qa-cloud';
    const cat = POOL.map((name) => (rand() < 0.5 ? longRow(name, { longLoaded: rand() < 0.3, loaded: rand() < 0.2 }) : row(name, { loaded: rand() < 0.2 })));
    const second = rand() < 0.2 ? pick([null, cat.slice(0, int(1, 6)), [...cat, 'QA text row'], cat.map((r) => ({ ...r, longLoaded: !r.longLoaded }))]) : cat;
    const toolsOk = {};
    for (const m of POOL) if (rand() < 0.5) toolsOk[m] = rand() < 0.4;
    for (const m of POOL) if (rand() < 0.2) toolsOk[`${m}-long`] = rand() < 0.5;
    const measured = {};
    for (const m of POOL) if (rand() < 0.2) measured[m] = pick([1500, 3000, 99999]);
    for (const m of POOL) if (rand() < 0.1) measured[`${m}-long`] = pick([1500, 3000]);
    const role = pick(['fast', 'smart', 'code', 'fast', 'smart']);
    await addTurn(`random_${n}`, {
      ...(project ? { project } : {}), roles, catalogues: [cat, second], toolsOk, measured, loaded: rand() < 0.8 ? pick(POOL) : null,
      classification: rand() < 0.2 ? role : decision(role),
      gate: rand() < 0.7 ? NEEDS : pick([null, NONE, { decision: 'web_search' }]),
      ...(rand() < 0.08 ? { props: false } : {}), ...(rand() < 0.08 ? { managerEnabled: false } : {}),
      message: pick(MESSAGES), ...(rand() < 0.5 ? { body: { turnToolboxes: pick([['qa-many'], ['nextcloud-notes'], ['qa-many', 'web-search']]) } } : {}),
    });
  }
  delete process.env.NOEVIA_CONTRACT_FIXED_NOW;

  // ── Output ──────────────────────────────────────────────────────────────────────────────
  const facts = { FALLBACK_ROLES: toolModes.FALLBACK_ROLES, DIARY_EXTRAS_ID: diaryExtras.PROJECT_ID, providers: PROVIDERS,
    catalogue: { boxes: viaJson(TB.allToolboxes().map(boxRecord)), serverWrites: SERVER_WRITES, connected: TB.connectedBoxes({ id: 'qa-user' }) } };
  const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
  const text = `{"version":1,"generator":"crates/chat-prepare/tests/record_reroute.cjs",\n"node":${JSON.stringify(process.version)},\n`
    + `"sources":${JSON.stringify(sourcesRead())},\n"blocks":${JSON.stringify(blocks)},\n"facts":${JSON.stringify(facts)},\n`
    + `"turns":[\n${lines(turns)}\n],\n"differences":[]}\n`;
  const check = process.argv.indexOf('--check');
  const moved = turns.filter((t) => t.lines.some((l) => l.includes('cannot take tools'))).length;
  const switched = turns.filter((t) => t.status && t.status.id === 'loadingLongContext').length;
  const counts = `${turns.length} turns (${turns.filter((t) => t.throws).length} throw, ${turns.filter((t) => t.refused).length} refused, ${moved} moved, `
    + `${switched} long-context switches)`;
  if (check > 0) {
    const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
    if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
    process.stderr.write(`ok: ${counts} recorded as committed\n`, () => process.exit(0));
  } else {
    process.stderr.write(`recorded: ${counts}\n`);
    process.stdout.write(text, () => process.exit(0));
  }
})().catch((e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
