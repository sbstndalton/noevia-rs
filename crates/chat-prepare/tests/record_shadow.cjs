#!/usr/bin/env node
'use strict';
// Recorded hand-off frames for crates/chat-prepare (slice P7a): the shadow contract the Rust preparation of a whole
// free-chat turn is replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout with server/wasm/dav-parse.wasm built> \
//     node crates/chat-prepare/tests/record_shadow.cjs > crates/chat-prepare/tests/fixtures/node-shadow.v1.json
//   ... node crates/chat-prepare/tests/record_shadow.cjs --check crates/chat-prepare/tests/fixtures/node-shadow.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every expectation comes from noevia-core's own code; the inputs are synthetic and QA-named (hand-made edges plus a
// seeded PRNG), passed through JSON first so Node reads exactly what is recorded. Each case is one whole FREE-CHAT
// turn (no project) through chat.cjs createChatHandler().handleChat with the front's native hand-off requested
// (nativeHandoff: requested, capabilities {skills, once}, not supervising), as crates/chat-turn's record_turns.cjs
// asks for it. The turn is on the local default provider with no routing modes and no cloud route: Manual (no Auto
// roles, autoRoles() is null, the model the loaded one, lastLoadedModel) unless a case names `roles` and the
// classifier's `pick` (an Automatic turn). No external provider, no
// Inference machine, no tool gate, no framing reasoner, no brain context (its flag off), no provenance policy, no step
// supervision, the stream guard off (provider-egress.cjs default), no chat-wide grant, no tombstones. Such a turn ends
// after its preparation with a `prepared` frame (rust-chat-native.cjs preparedTurn), the last event of the stream;
// a turn rust-chat-native.cjs refusal() keeps in Node goes on there and the logged reason is recorded instead.
//
// A case's `input`, exactly as Node read it:
//   tz           process.env.TZ for the turn (the server's zone: current-time-part.cjs serverTimeZone, checked to
//                resolve to this very name)
//   nowMs        the frozen clock: Date.now() and `new Date()` answer this instant for the whole turn (so the
//                date line of current-time-part.cjs currentNow, and the frame's startedAt); NOEVIA_CONTRACT_FIXED_NOW
//                is unset
//   instructions the exact text of <workspace>/account-instructions.json (account-instructions.cjs read), or null when
//                there is no such file
//   memory       the exact text of <workspace>/account-memory.json (account-memory.cjs read), or null: no file
//   freeChats    what the handler's freeChats() answers (the user's free chat list; index.cjs FREE_CHATS), or null
//                when it answers an empty list
//   chatFraming  what chatFramingEnabled() answers (index.cjs: features.enabled('chatFraming'))
//   toolboxes    the toolbox catalogue allToolboxes() answers, as [{id, tools: [function names], writes: [the names
//                isWriteTool calls writes]}] (the history
//                replay's carryOrphanTool and knownTool read it: tools-mode.cjs isPublicToolName, PRIVATE_CORE_TOOLS)
//   settings     (left out: no settings database) the settings table's rows chat.cjs reads, key -> value (a missing
//                key: no row; null: SQL NULL): reasoning_effort_default and auto_sampling_presets_enabled
//   roles, pick  (left out: Manual) what autoRoles() answers and the classifier's verdict (an Automatic turn)
//   models       (left out: none) more loaded models besides the default one (distinct role and project models)
//   lastLoaded   what lastLoadedModel() answers (the model a Manual turn without a project model answers with)
//   projects     (left out: none) the account's stored projects, what getProject(id) finds (P7c): no skills
//   codeTasks    (left out: none) what codeTasksFor(project) answers (a project sharing into Chat)
//   body         the POST /api/chat body (no skill, turnToolboxes or compactOnly; a projectId on the P7c cases)
// HISTORY_CAP is 1000 (index.cjs). What a case records: `prepared` the whole frame or null, `refused` the logged
// refusal reason or null, `threw` the message of what handleChat threw or null.
// Each source file's sha256 is recorded, so a changed Node file needs a fresh recording. stdout is the fixture.

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
// The engine is a llama.cpp deployment's (reasoning-effort.cjs localEngine): the default provider is the local engine.
for (const key of Object.keys(process.env)) if (/_IMPL$/.test(key) || /^(CONTEXT_LOG|MODEL_MANAGER_KIND|INFERENCE_BASE_URL|NOEVIA_)/.test(key)) delete process.env[key];
const ENGINE = 'http://qa-engine.invalid';
process.env.MODEL_MANAGER_KIND = 'llamacpp';
process.env.INFERENCE_BASE_URL = ENGINE;
process.env.TZ = 'UTC';

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { EventEmitter } = require('node:events');

if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
if (!fs.existsSync(path.join(root, 'server', 'wasm', 'dav-parse.wasm'))) {
  process.stderr.write('Build server/wasm/dav-parse.wasm in the checkout first (tools/build-dav-parse-wasm.sh).\n');
  process.exit(2);
}
const logged = [];
for (const k of ['log', 'info', 'warn', 'error', 'debug']) {
  console[k] = (...a) => { const line = a.map(String).join(' '); logged.push(line); process.stderr.write(`${line}\n`); };
}
const READ_FILES = ['server/chat.cjs', 'server/index.cjs', 'server/rust-chat-native.cjs', 'server/account-instructions.cjs', 'server/account-memory.cjs',
  'server/current-time-part.cjs', 'server/chat-frame-steering.cjs', 'server/chat-framing.cjs', 'server/chat-sources.cjs', 'server/prompt-framing.cjs',
  'server/tools-mode.cjs', 'server/toolboxes.cjs', 'server/chat-title.cjs', 'server/project-edit-target.cjs', 'server/dav-parse.lock'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex');
function sourcesRead() {
  const server = `${path.join(root, 'server')}${path.sep}`;
  const loaded = Object.keys(require.cache).filter((k) => k.startsWith(server)).map((k) => path.relative(root, k).split(path.sep).join('/'));
  if (loaded.some((k) => k.includes('node_modules/'))) throw new Error('a module from node_modules was loaded');
  return Object.fromEntries([...new Set([...READ_FILES, ...loaded])].sort().map((f) => [f, sha(f)]));
}
const serverRequire = (f) => require(path.join(root, 'server', f));
const HISTORY_CAP = 1000;
const indexText = fs.readFileSync(path.join(root, 'server/index.cjs'), 'utf8');
for (const wiring of [`const HISTORY_CAP = ${HISTORY_CAP};`, "  chatFramingEnabled: () => features.enabled('chatFraming'), freeChats: () => Array.from(FREE_CHATS),",
  '  nativeHandoff: chatNativeHandoff,', "  brainContext: { enabled: () => features.enabled('brainContext') && features.enabled('chatFraming'),"]) {
  if (!indexText.includes(wiring)) throw new Error(`index.cjs no longer holds ${JSON.stringify(wiring)}`);
}
const toolboxesLib = serverRequire('toolboxes.cjs');
const { createDriveTools } = serverRequire('gdrive-tools.cjs');
const { missingRoles, staleRolesError } = serverRequire('auto-roles-check.cjs');
const diaryExtras = serverRequire('diary-extras.cjs');
const { createChatHandler } = serverRequire('chat.cjs');
const ACCOUNT_INSTRUCTIONS = 'account-instructions.json';
const ACCOUNT_MEMORY = 'account-memory.json';

let seed = 0x2026_1114 ^ 0x7a;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const pick = (list) => list[Math.floor(rand() * list.length)];
const chance = (p) => rand() < p;
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

// index.cjs MCP_INTERNAL_SERVER: noevia's own in-process MCP server.
const INTERNAL = 'noevia';
// The toolbox catalogue: core, the Drive connector box, a public web-search box and a private-data notes box (MCP).
const fn = (name, description) => ({ type: 'function', function: { name, description, parameters: { type: 'object', properties: { query: { type: 'string' } } } } });
const MCP = [
  { id: 'web-search', label: 'QA Web search', description: 'QA web search', source: 'mcp', server: 'qa-search', tools: [fn('web_search', 'Search the web (QA).')], reads: ['web_search'] },
  { id: 'qa-notes', label: 'QA notes', description: 'QA notes for QA', source: 'mcp', server: 'qa-notes',
    tools: [fn('qa_notes_search', 'Search QA notes.'), fn('qa_notes_save', 'Save a QA note.')], reads: ['qa_notes_search'] },
  // P7f review: the in-process server's project documents box (its two edit tools), and an outside box offering one of
  // those names too (declared a read there: the write declaration wins) and a read its server reports as a write.
  { id: 'project-docs', label: 'QA project documents', description: 'QA project documents', source: 'mcp', server: INTERNAL,
    tools: [fn('project_list_files', 'List QA files.'), fn('project_append_file', 'Append to a QA file.'), fn('project_replace_text', 'Replace QA text.')],
    reads: ['project_list_files'] },
  { id: 'qa-docs', label: 'QA docs', description: 'QA docs', source: 'mcp', server: 'qa-docs',
    tools: [fn('qa_docs_read', 'Read QA docs.'), fn('project_replace_text', 'Replace QA doc text.')], reads: ['qa_docs_read', 'project_replace_text'] },
];
// mcpState.tools: each name held by the first configured server offering it (the in-process server is first).
const MCP_TOOLS = new Map([['web_search', { readOnly: true, serverId: 'qa-search' }], ['qa_notes_search', { readOnly: true, serverId: 'qa-notes' }], ['qa_notes_save', { readOnly: false, serverId: 'qa-notes' }],
  ['project_list_files', { readOnly: true, serverId: INTERNAL }], ['project_append_file', { readOnly: false, serverId: INTERNAL }], ['project_replace_text', { readOnly: false, serverId: INTERNAL }],
  ['qa_docs_read', { readOnly: false, serverId: 'qa-docs' }]]);
const EDIT_TOOLS = serverRequire('project-edit-target.cjs').EDIT_TOOLS;
const driveTools = createDriveTools({ accounts: { forUser: () => ({ drive: { state: () => ({ state: 'none' }) } }) }, cap: 8000 });
// A coworker conversation's page save (specialist-page-save.cjs): offered by a `specialist-` project, asked about every time.
const TB = toolboxesLib.createToolboxes({ boxes: [driveTools.box], kiwixTools: null, driveTools, specialistTools: serverRequire('specialist-page-save.cjs').createPageSave({ store: {} }), mcpBoxes: () => MCP, mcpTools: () => MCP_TOOLS, offered: () => true,
  prefill: { budgetFor: () => null, rateFor: () => 0.5 } });
// Each box's ids and tool names, and (P7f) the ones chat.cjs's isWriteTool calls writes.
const TOOLBOXES = TB.allToolboxes().map((b) => {
  const tools = (b.tools || []).map((t) => t.function.name);
  return { id: b.id, tools, writes: tools.filter((n) => TB.isWriteTool(n)) };
});
const PROVIDER = { id: 'default', label: 'QA engine', baseUrl: ENGINE };
const MODEL = 'qa-model-9b';
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-chat-shadow-'));
process.on('exit', () => fs.rmSync(tmp, { recursive: true, force: true }));

// The clock, frozen for a turn: Date.now() and an argument-less `new Date()` answer `frozenAt`.
const RealDate = Date;
let frozenAt = 0;
class FrozenDate extends RealDate {
  constructor(...args) { if (args.length) super(...args); else super(frozenAt); }
  static now() { return frozenAt; }
}

let current = null;
// The server holding a tool name for the current case.
const holderOf = (name) => ((current.spec.holders || {})[name] ?? MCP_TOOLS.get(name)?.serverId);
// The names the in-process server holds for the current case (recorded as input `inApp`).
const inAppNames = () => [...new Set([...MCP_TOOLS.keys(), ...Object.keys(current.spec.holders || {})])].filter((n) => holderOf(n) === INTERNAL).sort();
function handlerFor(dir) {
  const S = () => current.spec, L = () => current.log;
  let uuid = 0;
  const qaCrypto = Object.assign(Object.create(crypto), { randomUUID: () => `00000000-0000-4000-8000-${String(uuid += 1).padStart(12, '0')}` });
  // Asked only by a turn that stays in Node: one short reply, no tool call.
  const fetch = async (url, init) => {
    L().fetches += 1;
    if (!String(url).startsWith(ENGINE)) throw new Error('QA: no network');
    void init;
    return { ok: true, status: 200, headers: new Map(), body: (async function* () { yield Buffer.from(`data: ${JSON.stringify({ choices: [{ delta: { content: 'QA ok' } }] })}\n\ndata: [DONE]\n\n`); })() };
  };
  return createChatHandler({
    modelManager: { enabled: true, baseUrl: ENGINE,
      health: async () => ({ ok: true, body: { all_models_loaded: [MODEL, ...(S().models || [])].map((model_name) => ({ model_name, loaded: true, recipe_options: { ctx_size: 32768 } })) } }),
      acquireLease: async (model) => ({ model, headers: () => ({}), release: async () => {}, fetch: (base) => base }),
      props: async () => ({ ok: false }), inferenceBusy: () => false, load: async () => ({ ok: true }) },
    templateToolsGate: { allowsTools: async () => true, markUnsupported() {} }, toolGate: null, routingModes: { enabled: () => false },
    reasoningEffort: serverRequire('reasoning-effort.cjs'), crypto: qaCrypto, path, fs, fetch,
    // The settings table chat.cjs reads (`authService?.db?.prepare(...).get()?.value`): the case's rows, or no
    // database at all when the case names none (`settings` left out).
    authService: { audit() {}, diaryEnabled: () => false,
      get db() {
        const rows = S().settings;
        if (rows === undefined) return undefined;
        return { prepare: (sql) => ({ get: () => {
          const key = /WHERE key='([a-z_]+)'/.exec(sql)?.[1];
          if (!key) throw new Error(`QA: unexpected settings query ${sql}`);
          return Object.prototype.hasOwnProperty.call(rows, key) ? { value: rows[key] } : undefined;
        } }) };
      } },
    HISTORY_CAP, DEFAULT_PROVIDER_ID: 'default', createToolExchange: () => async () => 'unused',
    currentWorkspace: () => ({ userId: 'qa-user', dir, assetDir: () => path.join(dir, 'qa-assets') }),
    // The case's stored projects (getProject reads the account's projects.json; P7c).
    getProject: (id) => structuredClone((S().projects || []).find((p) => p && p.id === id)), skillsIndexFor: () => [],
    freeChats: () => structuredClone(S().freeChats || []),
    chatFramingEnabled: () => S().chatFraming,
    getProvider: (id) => (id === 'default' ? { ...PROVIDER } : null),
    providerHeaders: (p, extra) => ({ 'Content-Type': 'application/json', ...(extra || {}) }),
    // Automatic (P7b review): the case's roles, and its classifier verdict; Manual without them.
    autoRoles: () => structuredClone(S().roles ?? null),
    visionDescriptions: new Map(), visionProbe: async () => ({ supported: false, reason: 'qa' }),
    chatSkillRouter: { select: async () => ({ loaded: [] }) }, oauthServerIds: () => new Set(), accountReady: () => true,
    chatToolRouter: { select: async (ids) => ({ ids, routed: false }) },
    brainContext: { enabled: () => false, chats: () => [], read: () => null, maxChars: () => 2000 },
    framingReasoner: null, provenancePolicy: null, stepSupervision: null, chatMachines: null,
    DEFAULT_TOOLBOXES: TB.DEFAULT_TOOLBOXES, CONNECTOR_BOXES: TB.CONNECTOR_BOXES, connectedBoxes: TB.connectedBoxes, allToolboxes: TB.allToolboxes,
    resolveTools: TB.resolveTools, isWriteTool: TB.isWriteTool,
    toolPolicy: { mode: () => 'ask' }, requestScope: { getStore: () => ({ workspace: { userId: 'qa-user' }, authn: { user: { id: 'qa-user', role: 'member' } } }) },
    rag: { filesContext: async () => null }, prefill: { recordSample() {} }, reduceToolResult: () => ({ text: 'reduced' }), diaryExtras,
    DIARY_BASE: 'http://qa-diary.invalid', TOOL_RESULT_CAP: 8000,
    json: (res, status, body) => { L().http = { status, body }; res.end(); },
    saveChats() {}, endpointApproved: () => true, diaryHeaders: () => ({}),
    // The loaded models: the default one and the case's `models` (P7d: distinct role and project models).
    lastLoadedModel: () => MODEL, modelsInstalled: async () => [MODEL, ...(S().models || [])].map((name) => ({ name, labels: [], loaded: true })),
    servedCatalogue: async () => [MODEL, ...(S().models || [])].map((name) => ({ name, labels: [], loaded: true })),
    classifyFastOrSmart: async () => { if (!S().pick) throw new Error('QA: a Manual turn is not classified'); return S().pick; },
    missingRoles, staleRolesError,
    chatWideApproved: async () => null, recordUsage() {}, recordToolUse() {}, executeToolCall: async () => 'unused', awaitApproval: async () => 'deny',
    codeTasksFor: () => structuredClone(S().codeTasks || []), sourceJobs: { settle: async () => ({ notReady: [], aborted: false }) },
    chatTitles: { save() {}, autoNamed: () => true },
    grantContext: async () => ({ holder: { kind: 'absent' } }),
    // index.cjs: an edit tool held by the in-process server (a case's `holders` moves a name to another server).
    projectEditTool: (name) => EDIT_TOOLS.has(name) && holderOf(name) === INTERNAL,
    nativeHandoff: { requested: () => true, capabilities: () => new Set(['skills', 'once']), supervising: () => false },
  });
}

const DEFAULT_NOW = RealDate.UTC(2026, 10, 14, 12, 0, 0);
const CHAT = 'c-1794571200000-qa1x9z';
const turns = [];
async function addTurn(name, raw) {
  const { expectRefused, ...given } = raw;
  const spec = viaJson({ tz: 'UTC', nowMs: DEFAULT_NOW, instructions: null, memory: null, freeChats: null, chatFraming: false, body: req(), ...given });
  if (!Number.isSafeInteger(spec.nowMs)) throw new Error(`${name}: nowMs`);
  for (const text of [spec.instructions, spec.memory]) if (text !== null && (typeof text !== 'string' || !text.isWellFormed())) throw new Error(`${name}: a file text must be well-formed`);
  const dir = fs.mkdtempSync(path.join(tmp, 'qa-'));
  if (spec.instructions !== null) fs.writeFileSync(path.join(dir, ACCOUNT_INSTRUCTIONS), spec.instructions, 'utf8');
  if (spec.memory !== null) fs.writeFileSync(path.join(dir, ACCOUNT_MEMORY), spec.memory, 'utf8');
  process.env.TZ = spec.tz;
  const resolved = new Intl.DateTimeFormat().resolvedOptions().timeZone;
  if (resolved !== spec.tz) throw new Error(`${name}: TZ ${spec.tz} resolves to ${resolved}`);
  const log = { http: null, fetches: 0 };
  current = { spec, log };
  const events = [];
  const res = new EventEmitter();
  res.writableEnded = false; res.destroyed = false;
  res.writeHead = () => {}; res.setHeader = () => {};
  res.write = (line) => {
    const s = String(line);
    if (s.startsWith(': ')) return true;
    if (!s.startsWith('data: ') || !s.endsWith('\n\n')) throw new Error(`${name}: unexpected frame ${JSON.stringify(s.slice(0, 200))}`);
    events.push(JSON.parse(s.slice(6)));
    return true;
  };
  res.end = () => { if (res.writableEnded) return; res.writableEnded = true; process.nextTick(() => { res.emit('finish'); res.emit('close'); }); };
  const { handleChat } = handlerFor(dir);
  const body = viaJson(spec.body);
  const input = { tz: spec.tz, nowMs: spec.nowMs, instructions: spec.instructions, memory: spec.memory,
    freeChats: spec.freeChats && spec.freeChats.length ? spec.freeChats : null, chatFraming: spec.chatFraming, toolboxes: TOOLBOXES,
    ...(spec.settings === undefined ? {} : { settings: spec.settings }),
    ...(spec.roles === undefined ? {} : { roles: spec.roles, pick: spec.pick }), ...(spec.models === undefined ? {} : { models: spec.models }),
    lastLoaded: MODEL, inApp: inAppNames(),
    ...(spec.projects === undefined ? {} : { projects: spec.projects }), ...(spec.codeTasks === undefined ? {} : { codeTasks: spec.codeTasks }),
    body: viaJson(spec.body) };
  const from = logged.length;
  let threw = null;
  const random = Math.random;
  frozenAt = spec.nowMs;
  global.Date = FrozenDate;
  Math.random = () => 0.5;
  try { await handleChat({ headers: {} }, res, body, { user: { id: 'qa-user', role: 'member' } }); await new Promise((resolve) => setImmediate(resolve)); }
  catch (e) { threw = String(e && e.message || e); process.stderr.write(`QA ${name} threw: ${e && e.stack}\n`); }
  finally { global.Date = RealDate; Math.random = random; }
  const last = events[events.length - 1];
  const prepared = last && last.type === 'prepared' ? last : null;
  const refused = logged.slice(from).map((l) => /^\[chat\] native executor not used for this turn: (.+)$/.exec(l)?.[1]).find(Boolean) || null;
  if (log.http && !refused) throw new Error(`${name}: answered ${log.http.status} ${JSON.stringify(log.http.body)} before the hand-off`);
  if (prepared && log.fetches) throw new Error(`${name}: the engine was asked before the hand-off`);
  if (!prepared && !refused && !threw) throw new Error(`${name}: no prepared frame, no refusal; events: ${JSON.stringify(events).slice(0, 600)}`);
  if (expectRefused !== undefined && expectRefused !== refused) throw new Error(`${name}: refused ${refused}, expected ${expectRefused}`);
  if (expectRefused === undefined && !prepared) throw new Error(`${name}: not handed off (${refused || threw})`);
  turns.push({ name, input, prepared, refused, threw });
}
// `expectRefused`: a case made to stay in Node, with the reason it must be refused for (never recorded as input).
const refusedTurn = (name, raw, reason) => addTurn(name, { ...raw, expectRefused: reason });

// The instant `zone` turns to day y-m-d (its local midnight), found by stepping a quarter hour at a time.
function localMidnight(zone, y, m, d) {
  const f = new Intl.DateTimeFormat('en-US', { timeZone: zone, year: 'numeric', month: 'numeric', day: 'numeric', hour: 'numeric', minute: 'numeric', second: 'numeric', hourCycle: 'h23' });
  for (let q = -15 * 4; q <= 15 * 4; q += 1) {
    const t = RealDate.UTC(y, m - 1, d) - q * 15 * 60000;
    const p = Object.fromEntries(f.formatToParts(new RealDate(t)).map((x) => [x.type, x.value]));
    if (Number(p.year) === y && Number(p.month) === m && Number(p.day) === d && Number(p.hour) === 0 && Number(p.minute) === 0 && Number(p.second) === 0) return t;
  }
  throw new Error(`no midnight for ${zone} ${y}-${m}-${d}`);
}

const ZONES = ['UTC', 'Europe/Berlin', 'America/New_York', 'Asia/Tokyo', 'Australia/Adelaide', 'Pacific/Chatham'];
const DAYS = [[2026, 11, 14], [2026, 11, 15], [2026, 12, 31], [2027, 1, 1], [2027, 1, 4], [2026, 3, 29], [2026, 10, 25], [2028, 2, 29], [2026, 6, 15], [2029, 12, 31]];
const CHAT_IDS = [CHAT, 'c-1794657600000-qa7k2m', 'c-1794744000000-qa0p4q'];
const OTHER_CHAT = 'c-1794830400000-qa9z9z';

// Messages: ASCII, non-ASCII, astral characters, lone surrogates, control characters, a long one.
const MESSAGES = [
  'QA hello', 'QA what is the plan for today?', 'QA write a short poem about QA testing', 'QA fix this: `const qa = 1;`',
  'QA Grüße aus Zürich – wie spät ist es? ½ € ß', 'QA 今日は何曜日ですか？', 'QA مرحبا، كيف حالك؟', 'QA é vs é (NFC and NFD)',
  'QA 😀 𝔘𝔫𝔦𝔠𝔬𝔡𝔢 🇩🇪 👩‍👩‍👧', 'QA lone high \ud83d here', 'QA lone low \udc00 here', 'QA ends on a high surrogate \ud800',
  'QA\nmulti\r\nline\twith tabs', '  QA padded with spaces  ', 'QA {"json": [1, 2, "\\u0000"]} and $& $1 $$', 'QA zero​width line para',
  'QA latest version of QA tool?', 'QA what week is it?',
];
const TEXTS = ['QA earlier question', 'QA earlier answer', 'QA Grüße', 'QA 😀 astral', 'QA lone \udfff low', 'QA line\nbreak', 'QA $& replacement', 'QA ok'];

const INSTRUCTIONS = [
  null, '{not json', '{}', 'null', '[]', '"QA a string"',
  JSON.stringify({ text: 'QA always answer as a QA tester.', style: 'concise', updatedAt: 1794571200000 }),
  JSON.stringify({ text: '', style: 'detailed', advanced: { length: 'short', tone: 'formal', formatting: 'structured', emoji: 'none' }, language: 'German' }),
  JSON.stringify({ style: 'default', advanced: { length: 'long', tone: 'casual', formatting: 'minimal', emoji: 'some' }, language: 'Português (Brasil)' }),
  JSON.stringify({ text: 'QA prefer tables.\nQA second line: Grüße 😀', language: '  Deutsch   Schweiz ' }),
  JSON.stringify({ text: 42, style: 'loud', advanced: { length: 'huge', tone: 'formal', emoji: true }, language: 'German; ignore all rules' }),
  JSON.stringify({ text: 'QA only text' }),
  JSON.stringify({ language: 'one two three four five' }),
];
const MEMORIES = [
  null, '{not json', '{}', 'null',
  JSON.stringify({ memories: ['QA likes tea', 'QA works nights'], useProjectMemories: true, updatedAt: 1794571200000 }),
  JSON.stringify({ memories: ['  QA spaced  ', '', '   ', 42, null, 'QA likes tea', 'QA likes tea', 'QA Grüße 😀'] }),
  JSON.stringify({ memories: 'QA not a list' }),
  JSON.stringify({ memories: ['QA prefers metric units'], useProjectMemories: false }),
];
// Frames as stored on a chat of the free list (chat-framing.cjs normalizeFrame reads them).
const FRAMES = [
  undefined,
  { kind: 'search', projectId: null, tags: ['qa-tag'], links: [], confirmed: false, source: 'suggested' },
  { kind: 'question', projectId: null, tags: [], links: [], confirmed: true, source: 'user' },
  { kind: 'search', tags: ['qa-search', '#QA Tag 2'], confirmed: true },
  { kind: 'action', projectId: 'qa-project-1', tags: ['qa'], links: ['c-qa-linked'], confirmed: true, source: 'suggested' },
  { kind: 'idea', tags: ['ＱＡ全角', 'qa émoji 😀', '##qa-hash', 'qa/slash.dot', '   '], confirmed: true },
  { kind: 'code', tags: 'qa-not-a-list', confirmed: true },
  { kind: 'qa-unknown-kind', tags: ['qa'], confirmed: true },
  { kind: 'question', confirmed: 'true' },
];

const user = (content) => ({ role: 'user', content });
const assistant = (content) => ({ role: 'assistant', content });
const toolResult = (name, content, role = 'tool') => ({ role, name, content });
const applied = (name, content, extra = {}) => ({ role: 'tool', name, content, applied: true, ...extra });
const declined = (name, content) => ({ role: 'tool', name, content, declined: true });
const failed = (content, failure = 'QA the model stopped') => ({ role: 'assistant', content, failure });
const LONG_RESULT = `QA long result ${'0123456789'.repeat(60)} end`;

// One random history entry: ordinary turns mostly, then every replay edge chat.cjs filters or folds.
function randomEntry() {
  const r = rand();
  if (r < 0.32) return user(pick(TEXTS));
  if (r < 0.6) return assistant(pick(TEXTS));
  if (r < 0.64) return { role: pick(['user', 'assistant']), content: '' };
  if (r < 0.68) return failed(pick(['QA partial', '']), pick(['QA stopped', '   ', 42]));
  if (r < 0.74) return applied(pick(['qa_notes_save', 'drive_create_file', 'web_search', 'qa bad name!', 'qa_unknown_write']), pick(['QA saved', '']),
    { ...(chance(0.6) ? { target: pick(['QA target', '', 'x'.repeat(320)]) } : {}), ...(chance(0.6) ? { args: pick(['{"qa":1}', '{"b":2,"a":1}', 'not json', '']) } : {}) });
  if (r < 0.78) return declined(pick(['qa_notes_save', 'drive_create_file', 'qa_unknown_write']), pick(['QA declined', LONG_RESULT]));
  if (r < 0.9) return toolResult(pick(['web_search', 'qa_unknown_tool', 'qa_notes_search', 'read_project_file', 'get_current_time']), pick(['QA result', LONG_RESULT, 'QA 😀']), pick(['tool', 'tool', 'function']));
  if (r < 0.92) return { role: 'tool', content: 'QA nameless result' };
  return pick([null, 7, 'QA a string', [], { role: 'system', content: 'QA system' }, { role: 'user', content: 42 }, { role: 'user' }, { content: 'QA no role' },
    { role: 'assistant', content: ['QA', 'parts'] }, { role: 'user', content: 'QA applied user', applied: true }]);
}

const req = (extra = {}) => ({ message: 'QA hello', spaceId: 'free', chatId: CHAT, ...extra });
const freeList = (frame, extra = {}) => [{ id: CHAT, title: 'QA chat', updatedAt: 1794571200000, ...(frame === undefined ? {} : { frame }), ...extra }];
const longHistory = (n, at = (i) => (i % 2 ? assistant(`QA a${i}`) : user(`QA u${i}`))) => Array.from({ length: n }, (_, i) => at(i));

(async () => {
  // ── Messages and the request's ids ─────────────────────────────────────────────────────────────
  await addTurn('plain_utc', { body: req() });
  await addTurn('no_chat_id', { body: { message: 'QA hello', spaceId: 'free' } });
  await addTurn('no_space_id', { body: { message: 'QA hello', chatId: CHAT } });
  await addTurn('no_ids', { body: { message: 'QA hello' } });
  await addTurn('empty_chat_id', { body: req({ chatId: '' }) });
  await addTurn('chat_id_not_context_shaped', { body: req({ chatId: 'c-qa.1/2' }) });
  await addTurn('message_non_ascii', { body: req({ message: MESSAGES[4] }) });
  await addTurn('message_astral', { body: req({ message: MESSAGES[8] }) });
  await addTurn('message_lone_high_surrogate', { body: req({ message: MESSAGES[9] }) });
  await addTurn('message_lone_low_surrogate', { body: req({ message: MESSAGES[10] }) });
  await addTurn('message_controls_and_separators', { body: req({ message: MESSAGES[15] }) });
  await addTurn('message_long', { body: req({ message: `QA long ${'abcdefghij'.repeat(600)} 😀` }) });
  // A chat id that is not text stays in Node too (chat.cjs's permission checks).
  await refusedTurn('chat_id_number_refused', { body: req({ chatId: 1794571200000 }) }, 'chat-id');

  // ── The clock and the zone: midnights, ISO weeks, a leap day, a DST change ────────────────────────
  const mid = (zone, y, m, d) => localMidnight(zone, y, m, d);
  await addTurn('tz_berlin_before_midnight', { tz: 'Europe/Berlin', nowMs: mid('Europe/Berlin', 2026, 11, 15) - 1 });
  await addTurn('tz_berlin_midnight', { tz: 'Europe/Berlin', nowMs: mid('Europe/Berlin', 2026, 11, 15) });
  await addTurn('tz_new_york_midnight', { tz: 'America/New_York', nowMs: mid('America/New_York', 2026, 11, 15) });
  await addTurn('tz_new_york_before_midnight_new_year', { tz: 'America/New_York', nowMs: mid('America/New_York', 2027, 1, 1) - 1 });
  await addTurn('tz_tokyo_before_midnight', { tz: 'Asia/Tokyo', nowMs: mid('Asia/Tokyo', 2026, 11, 15) - 1 });
  await addTurn('tz_tokyo_new_year', { tz: 'Asia/Tokyo', nowMs: mid('Asia/Tokyo', 2027, 1, 1) });
  await addTurn('tz_adelaide_before_midnight', { tz: 'Australia/Adelaide', nowMs: mid('Australia/Adelaide', 2026, 11, 15) - 1 });
  await addTurn('tz_adelaide_midnight', { tz: 'Australia/Adelaide', nowMs: mid('Australia/Adelaide', 2026, 11, 15) });
  await addTurn('tz_chatham_midnight', { tz: 'Pacific/Chatham', nowMs: mid('Pacific/Chatham', 2026, 11, 15) });
  await addTurn('tz_chatham_before_midnight', { tz: 'Pacific/Chatham', nowMs: mid('Pacific/Chatham', 2026, 11, 15) - 1 });
  await addTurn('utc_iso_week_53', { nowMs: RealDate.UTC(2026, 11, 31, 23, 59, 59, 999) });
  await addTurn('utc_iso_year_differs', { nowMs: RealDate.UTC(2027, 0, 1, 0, 0, 0) });
  await addTurn('utc_leap_day', { nowMs: RealDate.UTC(2028, 1, 29, 12, 0, 0) });
  await addTurn('berlin_dst_spring', { tz: 'Europe/Berlin', nowMs: RealDate.UTC(2026, 2, 29, 1, 30, 0) });

  // ── Account instructions (account-instructions.json) ───────────────────────────────────────────
  await addTurn('instructions_concise_text', { instructions: INSTRUCTIONS[6] });
  await addTurn('instructions_detailed_advanced_language', { instructions: INSTRUCTIONS[7], tz: 'Europe/Berlin' });
  await addTurn('instructions_advanced_unicode_language', { instructions: INSTRUCTIONS[8] });
  await addTurn('instructions_language_spaces', { instructions: INSTRUCTIONS[9] });
  await addTurn('instructions_bad_values', { instructions: INSTRUCTIONS[10] });
  await addTurn('instructions_language_five_words', { instructions: INSTRUCTIONS[12] });
  await addTurn('instructions_unreadable', { instructions: '{not json' });
  await addTurn('instructions_empty_object', { instructions: '{}' });
  await addTurn('instructions_json_null', { instructions: 'null' });
  await addTurn('instructions_bom', { instructions: `﻿${INSTRUCTIONS[6]}` });
  // MAX_CHARS cuts between the two halves of an astral character: the system text keeps a lone high surrogate.
  await addTurn('instructions_cut_splits_surrogate', { instructions: JSON.stringify({ text: `${'Q'.repeat(3999)}😀 QA tail` }) });

  // ── Account memory (account-memory.json) ──────────────────────────────────────────────────────
  await addTurn('memory_basic', { memory: MEMORIES[4] });
  await addTurn('memory_unreadable', { memory: '{not json' });
  await addTurn('memory_empty_object', { memory: '{}' });
  await addTurn('memory_trim_dedupe', { memory: MEMORIES[5] });
  await addTurn('memory_caps', { memory: JSON.stringify({ memories: [...Array.from({ length: 55 }, (_, i) => `QA fact ${i}`), `${'M'.repeat(299)}😀 QA tail`] }) });
  await addTurn('memory_long_item_splits_surrogate', { memory: JSON.stringify({ memories: [`${'M'.repeat(299)}😀 QA tail`] }) });
  await addTurn('all_parts', { instructions: INSTRUCTIONS[7], memory: MEMORIES[4], chatFraming: true, freeChats: freeList(FRAMES[3]), tz: 'Pacific/Chatham',
    body: req({ history: [user('QA first'), assistant('QA reply')] }) });

  // ── Chat framing: the stored frame of the chat in the free list ────────────────────────────────
  await addTurn('frame_none', { chatFraming: true, freeChats: freeList(undefined) });
  await addTurn('frame_unconfirmed', { chatFraming: true, freeChats: freeList(FRAMES[1]) });
  await addTurn('frame_confirmed_question', { chatFraming: true, freeChats: freeList(FRAMES[2]) });
  await addTurn('frame_confirmed_search_tags', { chatFraming: true, freeChats: freeList(FRAMES[3]) });
  await addTurn('frame_confirmed_action_project', { chatFraming: true, freeChats: freeList(FRAMES[4]) });
  await addTurn('frame_confirmed_weird_tags', { chatFraming: true, freeChats: freeList(FRAMES[5]) });
  await addTurn('frame_confirmed_many_tags', { chatFraming: true, freeChats: freeList({ kind: 'idea', confirmed: true,
    tags: Array.from({ length: 22 }, (_, i) => `qa-tag-number-${i}-${'t'.repeat(40)}`) }) });
  await addTurn('frame_confirmed_flag_off', { chatFraming: false, freeChats: freeList(FRAMES[3]) });
  await addTurn('frame_confirmed_no_chat_id', { chatFraming: true, freeChats: freeList(FRAMES[3]), body: { message: 'QA hello', spaceId: 'free' } });
  await addTurn('frame_other_chat_only', { chatFraming: true, freeChats: [{ id: OTHER_CHAT, title: 'QA other', frame: FRAMES[2] }, { id: CHAT, title: 'QA chat' }] });
  await addTurn('frame_duplicate_ids_first_wins', { chatFraming: true, freeChats: [null, { id: CHAT, title: 'QA first' }, { id: CHAT, title: 'QA second', frame: FRAMES[2] }] });
  await addTurn('frame_unknown_kind', { chatFraming: true, freeChats: freeList(FRAMES[7]) });
  await addTurn('frame_confirmed_not_boolean', { chatFraming: true, freeChats: freeList(FRAMES[8]) });

  // ── History: what chat.cjs keeps, folds and drops before the new message ───────────────────────
  await addTurn('history_basic', { body: req({ history: [user('QA first'), assistant('QA reply'), user('QA second'), assistant('QA reply 2')] }) });
  await addTurn('history_empty_contents', { body: req({ history: [user(''), user('QA kept'), assistant(''), assistant('QA answer')] }) });
  await addTurn('history_failed_reply_orphans', { body: req({ history: [user('QA search this'), failed('QA partial text'), toolResult('web_search', 'QA web result'),
    toolResult('qa_unknown_tool', 'QA private'), toolResult('web_search', 'QA second web result'), toolResult('qa_notes_search', 'QA notes private'), user('QA and?'), assistant('QA so')] }) });
  await addTurn('history_failed_blank_failure_kept', { body: req({ history: [user('QA q'), failed('QA partial kept', '   ')] }) });
  await addTurn('history_applied_declined', { body: req({ history: [user('QA save it'), assistant('QA saving'),
    applied('qa_notes_save', 'QA saved', { target: 'QA target', args: '{"qa":1}' }), declined('drive_create_file', 'QA not run'), user('QA again'), assistant('QA done')] }) });
  await addTurn('history_applied_leading', { body: req({ history: [applied('qa_notes_save', '', { target: 'x'.repeat(320), args: 'y'.repeat(320) }), user('QA first'), assistant('QA ok')] }) });
  await addTurn('history_applied_bad_name_after_user', { body: req({ history: [user('QA do it'), applied('qa bad name!', 'QA saved'), declined('qa_unknown_write', LONG_RESULT)] }) });
  await addTurn('history_tool_fold_clip', { body: req({ history: [user('QA look'), assistant('QA looked'), toolResult('web_search', LONG_RESULT), toolResult('qa_notes_search', 'QA 😀', 'function')] }) });
  await addTurn('history_malformed', { body: req({ history: [null, 7, 'QA string', [], { role: 'system', content: 'QA system' }, { role: 'user', content: 42 }, { role: 'user' },
    { content: 'QA no role' }, user('QA real'), { role: 'assistant', content: ['QA', 'parts'] }, { role: 'tool', content: 'QA nameless' }, assistant('QA real reply')] }) });
  await addTurn('history_consecutive_users_ends_user', { body: req({ history: [user('QA one'), user('QA two'), assistant('QA a'), user('QA unanswered')] }) });
  await addTurn('history_starts_with_assistant', { body: req({ history: [assistant('QA orphan reply'), assistant('QA another'), user('QA q'), assistant('QA a')] }) });
  await addTurn('history_lone_surrogates', { body: req({ message: 'QA \ud800', history: [user('QA \udbff'), assistant('QA \udc00 x'), toolResult('web_search', 'QA \ud83d')] }) });
  await addTurn('history_cap_exact', { body: req({ history: longHistory(HISTORY_CAP) }) });
  await addTurn('history_cap_cuts_one', { tz: 'Asia/Tokyo', body: req({ history: longHistory(HISTORY_CAP + 1) }) });
  await addTurn('history_cap_after_filter', { body: req({ history: longHistory(1150, (i) => (i % 7 === 3 ? user('') : i % 11 === 5 ? failed(`QA f${i}`) : i % 2 ? assistant(`QA a${i}`) : user(`QA u${i}`))) }) });
  await addTurn('history_cap_cut_on_tool', { body: req({ history: longHistory(HISTORY_CAP + 3, (i) => (i < 4 ? user(`QA u${i}`) : i % 3 === 1 ? toolResult('web_search', `QA w${i}`) : i % 3 === 2 ? assistant(`QA a${i}`) : user(`QA u${i}`))) }) });

  // ── The settings rows the effort and the sampling presets read (P7b) ──────────────────────────
  const SETTINGS = [{}, { reasoning_effort_default: 'low' }, { reasoning_effort_default: 'high' }, { reasoning_effort_default: 'default' },
    { reasoning_effort_default: 'QA bogus' }, { reasoning_effort_default: null }, { auto_sampling_presets_enabled: 'false' },
    { auto_sampling_presets_enabled: 'true' }, { auto_sampling_presets_enabled: null }, { auto_sampling_presets_enabled: 'FALSE' },
    { reasoning_effort_default: 'high', auto_sampling_presets_enabled: 'false' }];
  for (const [k, settings] of SETTINGS.entries()) {
    for (const [j, message] of ['QA hello', 'Write a QA poem about the sea', 'Fix this QA function: def f(x): return x +', 'What is 17 * 23?'].entries()) {
      await addTurn(`settings_${k}_${j}`, { settings, body: req({ message }) });
    }
  }

  // ── Automatic turns (P7b review): the Fast route's lowered effort and thinking switch, the role's presets ──
  const ROLES = { fast: MODEL, smart: MODEL };
  for (const pick of ['fast', 'smart']) {
    for (const [k, settings] of [{}, { reasoning_effort_default: 'high' }, { reasoning_effort_default: 'low' }, { auto_sampling_presets_enabled: 'false' }].entries()) {
      for (const [j, message] of ['QA hello', 'Write a QA poem about the sea', 'Fix this QA function: def f(x): return x +'].entries()) {
        await addTurn(`auto_${pick}_${k}_${j}`, { roles: ROLES, pick, settings, body: req({ message }) });
      }
    }
  }
  await addTurn('auto_unassigned_pick', { roles: { fast: MODEL }, pick: 'smart', body: req() });
  // Applied writes: the same write twice and one whose arguments differ only in key order (one fingerprint).
  await addTurn('history_applied_dedupe', { body: req({ history: [user('QA q'), applied('append_project_file', 'QA done', { target: 'notes.md', args: '{"b":2,"a":1}' }),
    applied('append_project_file', 'QA done again', { target: 'notes.md', args: '{"a":1,"b":2}' }), applied('append_project_file', 'QA third', { target: 'notes.md', args: '{"a":1,"b":2}' }),
    applied('append_project_file', 'QA other', { target: 'other.md' }), applied('qa_tool', 'QA no args')] }) });

  // ── Project turns (P7c): a stored project with no files, skills or images ─────────────────────
  const PID = 'qa-proj-1';
  const PCHAT = 'p-1794571200000-qa2b7c';
  const project = (extra = {}) => ({ id: PID, name: 'QA Project', chats: [{ id: PCHAT, title: 'QA chat' }], files: [], ...extra });
  const preq = (extra = {}) => ({ message: 'QA hello', projectId: PID, chatId: PCHAT, ...extra });
  await addTurn('project_minimal', { projects: [project()], body: preq() });
  await addTurn('project_no_name', { projects: [project({ name: '' })], body: preq() });
  await addTurn('project_name_quoted', { projects: [project({ name: 'QA "Alpha" \u2019s <b>proj</b>' })], body: preq() });
  await addTurn('project_goal_instructions', { projects: [project({ goal: 'QA ship the launch notes', instructions: 'Answer in bullet points.\nCite the QA wiki.' })], body: preq() });
  await addTurn('project_memories', { memory: MEMORIES[1], projects: [project({ memories: ['QA prefers tea', '  QA works Fridays  ', 5, null, 'QA prefers tea'] })], body: preq() });
  await addTurn('project_memories_not_array', { projects: [project({ memories: 'QA one' })], body: preq() });
  await addTurn('project_account_parts', { instructions: INSTRUCTIONS[2], memory: MEMORIES[2], tz: 'Europe/Berlin', projects: [project({ goal: 'QA goal' })], body: preq() });
  await addTurn('project_no_chat_id', { projects: [project()], body: preq({ chatId: undefined }) });
  await addTurn('project_other_chat_id', { projects: [project()], body: preq({ chatId: 'p-1794571200000-qa9zzz' }) });
  await addTurn('project_effort_high', { projects: [project({ reasoningEffort: 'high' })], body: preq() });
  await addTurn('project_effort_bogus', { projects: [project({ reasoningEffort: 'QA bogus' })], settings: { reasoning_effort_default: 'low' }, body: preq() });
  await addTurn('project_sampling_explicit', { projects: [project({ sampling: { temperature: 0.3, top_p: 0.8 } })], body: preq({ message: 'Write a QA poem about the sea' }) });
  await addTurn('project_sampling_bad', { projects: [project({ sampling: { temperature: 'hot', top_k: -5, seed: 7 } })], body: preq({ message: 'Write a QA poem about the sea' }) });
  await addTurn('project_frame_confirmed', { chatFraming: true, projects: [project({ chats: [{ id: PCHAT, title: 'QA chat', frame: FRAMES.find((f) => f && f.confirmed) }] })], body: preq() });
  await addTurn('project_frame_flag_off', { chatFraming: false, projects: [project({ chats: [{ id: PCHAT, title: 'QA chat', frame: FRAMES.find((f) => f && f.confirmed) }] })], body: preq() });
  await addTurn('project_frame_scope', { chatFraming: true, projects: [project({ chats: [{ id: PCHAT, title: 'QA chat', frame: { ...FRAMES.find((f) => f && f.confirmed), projectId: PID } }] })], body: preq() });
  await addTurn('project_history', { projects: [project()], body: preq({ history: [user('QA q1'), assistant('QA a1'), applied('append_project_file', 'QA done', { target: 'notes.md', args: '{"path":"notes.md"}' }), toolResult('web_search', 'QA web')] }) });
  await addTurn('project_with_settings', { settings: { reasoning_effort_default: 'high', auto_sampling_presets_enabled: 'false' }, projects: [project()], body: preq({ message: 'Write a QA poem about the sea' }) });
  // P7d: the role's own model.
  const DISTINCT = { fast: 'qa-fast-4b', smart: 'qa-smart-12b', code: 'qa-code-7b' };
  for (const pick of ['fast', 'smart']) {
    await addTurn(`auto_distinct_${pick}`, { roles: DISTINCT, pick, models: Object.values(DISTINCT), body: req() });
    // A project is Automatic only with `routing: 'auto'` (chat.cjs wantsAuto); without it, Manual.
    await addTurn(`auto_distinct_${pick}_project`, { roles: DISTINCT, pick, models: Object.values(DISTINCT),
      projects: [project({ routing: 'auto' })], body: preq() });
    await addTurn(`project_manual_with_roles_${pick}`, { roles: DISTINCT, pick, models: Object.values(DISTINCT),
      projects: [project()], body: preq() });
  }
  await addTurn('auto_distinct_unassigned', { roles: { fast: 'qa-fast-4b' }, pick: 'smart', models: ['qa-fast-4b'], body: req() });
  await addTurn('project_own_model', { models: ['qa-project-9b'], projects: [project({ model: 'qa-project-9b' })], body: preq() });
  await addTurn('project_own_model_default', { projects: [project({ model: MODEL })], body: preq() });
  // P7f: a write tool offered (the QA notes box's save), from the turn's own pick and from the project's boxes.
  await addTurn('tools_turn_pick_notes', { body: req({ turnToolboxes: ['qa-notes'] }) });
  await addTurn('tools_project_notes', { projects: [project({ toolboxes: ['core', 'qa-notes', 'web-search'] })], body: preq() });
  await addTurn('tools_project_notes_message', { projects: [project({ toolboxes: ['qa-notes'] })], body: preq({ message: 'Save a QA note that says hello' }) });
  // P7f review: the in-process server's edit tools (editTools), a name in two boxes, a read its server calls a write,
  // and an edit tool name held by an outside server (not an edit tool for core).
  await addTurn('tools_project_docs', { projects: [project({ toolboxes: ['core', 'project-docs'] })], body: preq() });
  await addTurn('tools_project_docs_two_boxes', { projects: [project({ toolboxes: ['project-docs', 'qa-docs'] })], body: preq() });
  await addTurn('tools_project_docs_outside_holder', { holders: { project_append_file: 'qa-docs' }, projects: [project({ toolboxes: ['project-docs', 'qa-docs'] })], body: preq() });
  // The once card (#2446): a coworker project's page save, named in onceTools (the front says it runs that card).
  await addTurn('tools_coworker_page_save', { projects: [project({ id: 'specialist-qa-writer', name: 'QA Writer' })], body: preq({ projectId: 'specialist-qa-writer' }) });
  await addTurn('project_own_model_context_low', { models: ['qa-project-9b'], projects: [project({ model: 'qa-project-9b', contextProfile: 'low' })], body: preq() });
  await addTurn('project_context_high_skipped', { models: ['qa-project-9b'], projects: [project({ model: 'qa-project-9b', contextProfile: 'high' })], body: preq() });
  // Not covered yet (the shadow skips them): knowledge files, a project sharing into Chat, attached images.
  await addTurn('project_files_skipped', { projects: [project({ files: [{ name: 'qa.md', content: 'QA text', attachment: { state: 'ready' } }] })], body: preq() });
  await addTurn('project_shared_context_skipped', { projects: [project({ sharedContext: { chat: true } })], codeTasks: [{ id: 'qa-task-1', title: 'QA task', status: 'done', updatedAt: 1794571000000 }], body: preq() });
  await addTurn('project_assets_skipped', { projects: [project({ assets: [{ id: 'qa-img-1.png', name: 'qa.png', kind: 'image', mime: 'image/png' }] })], body: preq() });
  // A free chat whose attachments project exists: a project turn by its context project (skipped).
  await addTurn('free_chat_context_project_skipped', { projects: [{ id: `cowork-chat-context-${CHAT}`, name: 'QA hidden', chats: [], files: [{ name: 'qa.txt', content: 'QA', attachment: { state: 'ready' } }] }], body: req() });
  for (let n = 0; n < 40; n += 1) {
    const extra = {};
    if (chance(0.5)) extra.goal = pick(['QA goal', 'QA goal with "quotes"', '']);
    if (chance(0.5)) extra.instructions = pick(['QA be brief', 'QA line one\nQA line two', '']);
    if (chance(0.4)) extra.memories = pick([['QA m1'], ['QA m1', 'QA m2'], [], 'QA']);
    if (chance(0.3)) extra.reasoningEffort = pick(['low', 'high', 'default', 'QA']);
    if (chance(0.2)) extra.sampling = pick([{ temperature: 0.2 }, { top_p: 0.5, min_p: 0.05 }, {}]);
    const withChat = chance(0.8);
    const framed = chance(0.3);
    const p = project({ ...extra, ...(framed && withChat ? { chats: [{ id: PCHAT, title: 'QA chat', frame: pick(FRAMES) }] } : {}) });
    const history = Array.from({ length: chance(0.4) ? 0 : 1 + Math.floor(rand() * 6) }, randomEntry);
    await addTurn(`random_project_${n}`, { tz: pick(ZONES), instructions: pick(INSTRUCTIONS), memory: pick(MEMORIES), chatFraming: chance(0.6),
      ...(chance(0.4) ? { settings: pick(SETTINGS) } : {}), projects: [p],
      body: preq({ message: pick(MESSAGES), chatId: withChat ? PCHAT : undefined, ...(history.length ? { history } : {}) }) });
  }

  // ── Seeded random turns ──────────────────────────────────────────────────────────────────────
  for (let n = 0; n < 160; n += 1) {
    const tz = pick(ZONES);
    const [y, m, d] = pick(DAYS);
    const nowMs = chance(0.35) ? localMidnight(tz, y, m, d) + pick([-60000, -1, 0, 1, 60000]) : RealDate.UTC(y, m - 1, d) + Math.floor(rand() * 86400000);
    const chatId = chance(0.8) ? pick(CHAT_IDS) : undefined;
    const histLen = chance(0.3) ? 0 : 1 + Math.floor(rand() * 12);
    const history = Array.from({ length: histLen }, randomEntry);
    const listed = chance(0.75);
    const freeChats = listed ? [
      ...(chance(0.3) ? [{ id: OTHER_CHAT, title: 'QA other', frame: pick(FRAMES.filter(Boolean)) }] : []),
      ...(chatId ? [{ id: chatId, title: 'QA chat', ...(chance(0.7) ? { frame: pick(FRAMES) } : {}) }] : []),
    ] : null;
    const b = { message: pick(MESSAGES), ...(chance(0.85) ? { spaceId: 'free' } : {}), ...(chatId ? { chatId } : {}), ...(histLen || chance(0.5) ? { history } : {}) };
    const fields = { tz, nowMs, instructions: pick(INSTRUCTIONS), memory: pick(MEMORIES), freeChats: freeChats && freeChats.length ? freeChats : null,
      chatFraming: chance(0.6), body: b };
    // Drawn after everything above, so the earlier picks of each turn are unchanged.
    if (chance(0.5)) fields.settings = pick(SETTINGS);
    await addTurn(`random_${n}`, fields);
  }

  const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
  const text = `{"version":1,"generator":"crates/chat-prepare/tests/record_shadow.cjs",\n"node":${JSON.stringify(process.version)},\n`
    + `"sources":${JSON.stringify(sourcesRead())},\n"historyCap":${HISTORY_CAP},\n`
    + `"turns":[\n${lines(turns)}\n]}\n`;
  const counts = `${turns.length} turns (${turns.filter((t) => t.prepared).length} handed off, ${turns.filter((t) => t.refused).length} refused, `
    + `${turns.filter((t) => t.threw).length} threw), ${Buffer.byteLength(text)} bytes`;
  const check = process.argv.indexOf('--check');
  if (check > 0) {
    const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
    if (committed !== text) { process.stderr.write(`the recording differs from the committed fixture: record it again (${counts})\n`); process.exit(1); }
    process.stderr.write(`ok: ${counts} recorded as committed\n`, () => process.exit(0));
  } else {
    process.stderr.write(`recorded: ${counts}\n`);
    process.stdout.write(text, () => process.exit(0));
  }
})().catch((e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
