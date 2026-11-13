#!/usr/bin/env node
'use strict';
// Recorded routing modes for crates/chat-prepare (slice P5d, first part): the contract src/modes.rs is replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-prepare/tests/record_modes.cjs > crates/chat-prepare/tests/fixtures/node-modes.v1.json
//   ... node crates/chat-prepare/tests/record_modes.cjs --check crates/chat-prepare/tests/fixtures/node-modes.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every expectation comes from noevia-core's own code; the inputs are synthetic and QA-named (hand-made edges plus a
// seeded PRNG), passed through JSON first so Node reads exactly what is recorded:
//   turns  chat.cjs createChatHandler().handleChat, whole Automatic turns with the routingModes seams stubbed:
//          enabled, settings, sensitivity, openChoice (when the case wires it), awaitChoice, setChatFlags and log, plus
//          getProject/freeChats (the chat lists), getProvider, autoRoles and routing-modes.cjs preRule (wrapped: the
//          rules themselves are a later slice; the recorded answer is the real rule's). crypto.randomUUID answers
//          qa-choice-<n>. Recorded: those calls in order up to the meta event, the refusal (json), the route_pending and
//          route_remembered events (and the error event a refusal becomes once the stream is open), the meta event's model, route and routing, the `[routing]` line, and the reply
//          request's model and provider; or what handleChat threw.
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
const READ_FILES = ['server/chat.cjs', 'server/index.cjs', 'server/routing-modes.cjs'];
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
const routePreload = serverRequire('route-preload.cjs');
routePreload.startPreload = async ({ model }) => ({ model, use() {}, cancel() {}, releaseIfUnused: async () => {}, pending: () => false, settled: Promise.resolve() });

let seed = 0x2026_1113 ^ 0x5d;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const pick = (list) => list[Math.floor(rand() * list.length)];
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

const driveTools = createDriveTools({ accounts: { forUser: () => ({ drive: { state: () => ({ state: 'none' }) } }) }, cap: 8000 });
const TB = toolboxesLib.createToolboxes({ boxes: [driveTools.box], kiwixTools: null, driveTools, specialistTools: pageSave.createPageSave({ store: {} }),
  mcpBoxes: () => [], mcpTools: () => new Map(), offered: () => true, prefill: { budgetFor: () => null, rateFor: () => 0.5 } });
const PROVIDER = 'http://qa-default.invalid';
const PROVIDERS = {
  default: { id: 'default', label: 'QA local', baseUrl: PROVIDER },
  'qa-cloud': { id: 'qa-cloud', label: 'QA Cloud', baseUrl: `${PROVIDER}/qa-cloud`, shared: true },
  'qa-other': { id: 'qa-other-id', label: 'QA mismatched', baseUrl: `${PROVIDER}/qa-other`, shared: true },
};
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-chat-modes-'));
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
    sensitivity: (input) => { seam(['sensitivity', input.complete, input.chunks.length]); return settled(m().verdict); },
    awaitChoice: ({ id, userId, chatId }) => { seam(['awaitChoice', id, userId, chatId]); return settled(m().choice); },
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
    chatToolRouter: { select: async (ids) => ({ ids, routed: false }) },
    DEFAULT_TOOLBOXES: TB.DEFAULT_TOOLBOXES, CONNECTOR_BOXES: TB.CONNECTOR_BOXES, connectedBoxes: TB.connectedBoxes, allToolboxes: TB.allToolboxes,
    resolveTools: TB.resolveTools, isWriteTool: TB.isWriteTool,
    toolPolicy: { mode: () => 'allow' }, requestScope: { getStore: () => (S().noUser ? {} : { workspace: { userId: 'qa-user' } }) },
    rag: { filesContext: async () => null }, prefill: { recordSample() {} }, reduceToolResult: () => ({ text: 'reduced' }), diaryExtras,
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
  const spec = viaJson({ message: 'QA question', roles: ROLES, ...raw });
  process.env.NOEVIA_CONTRACT_FIXED_NOW = '2026-11-13T12:00:00Z';
  process.env.TZ = 'UTC';
  const log = { requests: [], calls: [], meta: false, refused: null, modesOn: false };
  log.seam = (entry) => { if (entry[0] === 'enabled') log.modesOn = true; if (!log.meta) log.calls.push(entry); };
  current = { spec, log, seam: log.seam };
  const rawEvents = [];
  const res = new EventEmitter();
  res.writeHead = () => {}; res.setHeader = () => {};
  res.write = (line) => { const s = String(line); rawEvents.push(s); if (s.includes('"type":"meta"')) log.meta = true; return true; };
  res.end = () => { res.writableEnded = true; res.emit('finish'); };
  const { handleChat } = handlerFor(fs.mkdtempSync(path.join(tmp, 'qa-')), spec);
  const body = { message: spec.message, ...(spec.project && !spec.free ? { projectId: spec.project.id } : {}), ...(spec.body || {}) };
  const from = logged.length;
  let thrown = null;
  // A project chat without an id gets one (chat.cjs `p-${Date.now()}-${Math.random()...}`): pinned, so it records the same.
  const [now, random] = [Date.now, Math.random];
  Date.now = () => 1763035200000; Math.random = () => 0.5;
  try { await handleChat({ headers: {} }, res, body, { user: { id: 'qa-user' } }); } catch (e) { thrown = e.constructor.name; process.stderr.write(`QA ${name} threw: ${e.stack}\n`); }
  finally { Date.now = now; Math.random = random; }
  await new Promise((resolve) => setImmediate(resolve));
  const events = rawEvents.join('').split('\n\n').filter((x) => x.startsWith('data: ')).map((x) => JSON.parse(x.slice(6)));
  const meta = events.find((e) => e.type === 'meta') || null;
  const routeEvents = events.filter((e) => ['route_pending', 'route_remembered'].includes(e.type) || (e.type === 'error' && !meta));
  const lines = logged.slice(from).filter((l) => l.startsWith('[routing]'));
  const reply = log.requests.find((r) => r.body.stream === true) || null;
  if (!thrown && !meta && !log.refused && !routeEvents.some((e) => e.type === 'error')) throw new Error(`${name}: no meta event and no refusal; events: ${rawEvents.join('').slice(0, 600)}`);
  turns.push({ name, spec, body, calls: log.calls, ...(thrown ? { throws: thrown } : {}), refused: log.refused, events: routeEvents,
    meta: meta ? { model: meta.model, chatId: meta.chatId ?? null, route: meta.route ?? null, routing: meta.routing ?? null } : null, lines,
    reply: reply ? { model: reply.body.model, url: reply.url } : null });
}

const AUTO = (extra = {}) => ({ id: 'qa-project-1', name: 'QA Project', routing: 'auto', chats: [], ...extra });
const CLOUD = { providerId: 'qa-cloud', fast: 'qa-cloud-fast', smart: 'qa-cloud-smart', code: '' };
const S = (mode, extra = {}) => ({ mode, whenSensitive: 'ask', cloud: CLOUD, ...extra });
const CLEAR = { flagged: false, flag: null };
const SENS = { flagged: true, flag: 'router' };
const CHAT = 'qa-chat-1';
const inProject = (flags) => AUTO({ chats: [{ id: CHAT, title: 'QA chat', ...flags }] });
const withChat = { body: { chatId: CHAT } };

(async () => {
  const P = AUTO();
  await addTurn('flag_off', { project: P, modes: { enabled: false } });
  await addTurn('enabled_not_true', { project: P, modes: { enabled: 'yes' } });
  await addTurn('settings_null', { project: P, modes: { enabled: true, settings: null } });
  await addTurn('no_mode', { project: P, modes: { enabled: true, settings: S(null) } });
  await addTurn('manual_never_routed', { project: { id: 'qa-project-1', name: 'QA Project', routing: 'manual', model: FAST }, modes: { enabled: true, settings: S('cloud') } });
  await addTurn('local_mode', { project: P, modes: { enabled: true, settings: S('local') } });
  await addTurn('local_mode_no_local_roles', { project: P, roles: { code: CODE }, modes: { enabled: true, settings: S('local') } });
  await addTurn('cloud_mode', { project: P, modes: { enabled: true, settings: S('cloud') } });
  await addTurn('cloud_mode_smart', { project: P, classification: { role: 'smart' }, modes: { enabled: true, settings: S('cloud') } });
  await addTurn('cloud_fast_only', { project: P, classification: { role: 'smart' }, modes: { enabled: true, settings: S('cloud', { cloud: { providerId: 'qa-cloud', fast: 'qa-cloud-fast', smart: '', code: '' } }) } });
  await addTurn('cloud_no_provider', { project: P, modes: { enabled: true, settings: S('cloud', { cloud: { providerId: '', fast: '', smart: 'x', code: '' } }) } });
  await addTurn('cloud_default_provider', { project: P, modes: { enabled: true, settings: S('cloud', { cloud: { ...CLOUD, providerId: 'default' } }) } });
  await addTurn('cloud_provider_mismatch', { project: P, modes: { enabled: true, settings: S('cloud', { cloud: { ...CLOUD, providerId: 'qa-other' } }) } });
  await addTurn('cloud_provider_missing', { project: P, modes: { enabled: true, settings: S('cloud', { cloud: { ...CLOUD, providerId: 'qa-gone' } }) } });
  await addTurn('cloud_no_models', { project: P, modes: { enabled: true, settings: S('cloud', { cloud: { providerId: 'qa-cloud', fast: '', smart: '', code: '' } }) } });
  await addTurn('cloud_no_models_force_local', { project: inProject({ forceLocal: true }), ...withChat, modes: { enabled: true, settings: S('cloud', { cloud: { providerId: '', fast: '', smart: '', code: '' } }) } });
  await addTurn('cloud_force_local', { project: inProject({ forceLocal: true }), ...withChat, modes: { enabled: true, settings: S('cloud') } });
  await addTurn('cloud_force_local_not_true', { project: inProject({ forceLocal: 'true' }), ...withChat, modes: { enabled: true, settings: S('cloud') } });
  await addTurn('cloud_diary_history', { project: P, body: { history: [{ role: 'user', content: 'QA q' }, { role: 'tool', name: 'Diary-search', content: 'QA entry' }] }, modes: { enabled: true, settings: S('cloud') } });
  await addTurn('cloud_diary_history_no_provider', { project: P, body: { history: [{ role: 'function', name: 'diary_read', content: 'QA entry' }] },
    modes: { enabled: true, settings: S('cloud', { cloud: { providerId: '', fast: '', smart: '', code: '' } }) } });
  await addTurn('cloud_diaryx_not_diary', { project: P, body: { history: [{ role: 'tool', name: 'diaryx', content: 'QA entry' }] }, modes: { enabled: true, settings: S('cloud') } });
  await addTurn('cloud_free_chat', { body: { chatId: CHAT, spaceId: 'free' }, freeChats: [{ id: CHAT, allowCloud: true }], modes: { enabled: true, settings: S('cloud') } });
  await addTurn('cloud_settings_cloud_null', { project: P, modes: { enabled: true, settings: { mode: 'cloud', whenSensitive: 'ask', cloud: null } } });
  await addTurn('settings_throws', { project: P, modes: { enabled: true, settings: { throws: true } } });
  await addTurn('hybrid_clear', { project: P, modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR } });
  await addTurn('hybrid_clear_allow_cloud', { project: inProject({ allowCloud: true }), ...withChat, modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR } });
  await addTurn('hybrid_no_cloud', { project: P, modes: { enabled: true, settings: S('hybrid', { cloud: { providerId: '', fast: '', smart: '', code: '' } }), verdict: CLEAR } });
  await addTurn('hybrid_ask_cloud', { project: P, ...withChat, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: { choice: 'cloud' } } });
  await addTurn('hybrid_ask_cloud_remember', { project: inProject({}), ...withChat, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: { choice: 'cloud', remember: true } } });
  await addTurn('hybrid_ask_local_remember', { project: inProject({}), ...withChat, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: { choice: 'local', remember: 1 } } });
  await addTurn('hybrid_ask_local_no_roles', { project: inProject({}), roles: { code: CODE }, ...withChat, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: { choice: 'local', remember: true } } });
  await addTurn('hybrid_remember_no_chat', { project: P, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: { choice: 'cloud', remember: true } } });
  await addTurn('hybrid_remember_free_chat', { body: { chatId: CHAT, spaceId: 'free' }, freeChats: [{ id: CHAT }], modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: { choice: 'local', remember: true } } });
  await addTurn('hybrid_set_flags_throws', { project: inProject({}), ...withChat, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: { choice: 'cloud', remember: true }, flagsThrow: true } });
  await addTurn('hybrid_timeout', { project: P, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: { choice: 'timeout' } } });
  await addTurn('hybrid_choice_null', { project: P, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: null } });
  await addTurn('hybrid_choice_text', { project: P, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: 'cloud' } });
  await addTurn('hybrid_await_throws', { project: P, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: { throws: true } } });
  await addTurn('hybrid_open_choice_true', { project: P, ...withChat, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, openChoice: true, choice: { choice: 'cloud' } } });
  await addTurn('hybrid_open_choice_false', { project: P, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, openChoice: false, choice: { choice: 'cloud' } } });
  await addTurn('hybrid_open_choice_throws', { project: P, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, openChoice: { throws: true }, choice: { choice: 'cloud' } } });
  await addTurn('hybrid_no_user', { project: P, noUser: true, modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: { choice: 'cloud' } } });
  await addTurn('hybrid_when_local', { project: P, modes: { enabled: true, settings: S('hybrid', { whenSensitive: 'local' }), verdict: SENS } });
  await addTurn('hybrid_when_other', { project: P, modes: { enabled: true, settings: S('hybrid', { whenSensitive: 'QA' }), verdict: SENS, choice: { choice: 'cloud' } } });
  await addTurn('hybrid_unavailable', { project: P, modes: { enabled: true, settings: S('hybrid'), verdict: { flagged: true, flag: 'unavailable', cause: 'not-qualified' } } });
  await addTurn('hybrid_unavailable_no_cause', { project: P, modes: { enabled: true, settings: S('hybrid'), verdict: { flagged: true, flag: 'unavailable' } } });
  await addTurn('hybrid_unavailable_allow', { project: inProject({ allowCloud: true }), ...withChat, modes: { enabled: true, settings: S('hybrid'), verdict: { flagged: true, flag: 'unavailable', cause: 'deadline' } } });
  await addTurn('hybrid_sensitive_allow', { project: inProject({ allowCloud: true }), ...withChat, modes: { enabled: true, settings: S('hybrid'), verdict: SENS } });
  await addTurn('hybrid_verdict_null', { project: P, modes: { enabled: true, settings: S('hybrid'), verdict: null } });
  await addTurn('hybrid_verdict_flagged_text', { project: P, modes: { enabled: true, settings: S('hybrid'), verdict: { flagged: 'no' } } });
  await addTurn('hybrid_verdict_flag_object', { project: P, modes: { enabled: true, settings: S('hybrid', { whenSensitive: 'local' }), verdict: { flagged: true, flag: { qa: 1 } } } });
  await addTurn('hybrid_verdict_throws', { project: P, modes: { enabled: true, settings: S('hybrid'), verdict: { throws: true } } });
  await addTurn('hybrid_log_throws', { project: P, modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR, logThrows: true } });
  await addTurn('hybrid_pre_message', { project: P, message: 'QA login: password = qa-hunter-22', modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR, choice: { choice: 'local' } } });
  await addTurn('hybrid_pre_history', { project: P, body: { history: [{ role: 'assistant', content: 'QA card 4111 1111 1111 1111' }] },
    modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR, choice: { choice: 'cloud' } } });
  await addTurn('hybrid_pre_force_local', { project: inProject({ forceLocal: true }), ...withChat, message: 'QA my diagnosis of asthma', modes: { enabled: true, settings: S('hybrid'), verdict: CLEAR } });
  await addTurn('hybrid_compact', { project: P, message: '', body: { compactOnly: true, chatId: 'qa-compact', history: [{ role: 'user', content: 'QA q1' }, { role: 'assistant', content: 'QA a1' }] },
    modes: { enabled: true, settings: S('hybrid'), verdict: SENS, choice: { choice: 'cloud' } } });
  await addTurn('weird_mode', { project: P, modes: { enabled: true, settings: S('QA'), verdict: CLEAR } });

  const MSGS = ['QA question', 'QA token: qa_live_0123456789abcdefghijklmnop', 'QA hi'];
  for (let n = 0; n < 80; n += 1) {
    const flags = pick([{}, { forceLocal: true }, { allowCloud: true }, { allowCloud: true, forceLocal: true }]);
    const free = rand() < 0.25;
    const mode = pick(['local', 'cloud', 'hybrid', 'hybrid', 'hybrid', null]);
    const cloud = pick([CLOUD, CLOUD, { providerId: '', fast: '', smart: '', code: '' }, { providerId: 'qa-cloud', fast: '', smart: '', code: 'qa-cloud-code' }]);
    const verdict = pick([CLEAR, SENS, { flagged: true, flag: 'unavailable', cause: 'partial' }, null]);
    const choice = pick([{ choice: 'cloud' }, { choice: 'cloud', remember: true }, { choice: 'local', remember: true }, { choice: 'local' }, { choice: 'timeout' }]);
    const roles = rand() < 0.15 ? { code: CODE } : ROLES;
    const chat = rand() < 0.7;
    await addTurn(`random_${n}`, {
      ...(free ? { freeChats: [{ id: CHAT, ...flags }] } : { project: inProject(flags) }), roles, message: pick(MSGS),
      body: { ...(chat ? { chatId: CHAT } : {}), ...(free ? { spaceId: 'free' } : {}) }, classification: { role: pick(['fast', 'smart', 'code']) },
      ...(rand() < 0.1 ? { noUser: true } : {}),
      modes: { enabled: rand() < 0.9, settings: S(mode, { cloud, whenSensitive: pick(['ask', 'ask', 'local']) }), verdict, choice,
        ...(rand() < 0.5 ? { openChoice: rand() < 0.85 } : {}) },
    });
  }
  delete process.env.NOEVIA_CONTRACT_FIXED_NOW;

  const facts = { DIARY_EXTRAS_ID: diaryExtras.PROJECT_ID, providers: PROVIDERS };
  const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
  const text = `{"version":1,"generator":"crates/chat-prepare/tests/record_modes.cjs",\n"node":${JSON.stringify(process.version)},\n`
    + `"sources":${JSON.stringify(sourcesRead())},\n"facts":${JSON.stringify(facts)},\n`
    + `"turns":[\n${lines(turns)}\n],\n"differences":[]}\n`;
  const check = process.argv.indexOf('--check');
  const counts = `${turns.length} turns (${turns.filter((t) => t.throws).length} throw, ${turns.filter((t) => t.refused).length} refused, `
    + `${turns.filter((t) => t.meta && t.meta.routing && t.meta.routing.route === 'cloud').length} cloud, ${turns.filter((t) => t.events.length).length} with route events)`;
  if (check > 0) {
    const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
    if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
    process.stderr.write(`ok: ${counts} recorded as committed\n`, () => process.exit(0));
  } else {
    process.stderr.write(`recorded: ${counts}\n`);
    process.stdout.write(text, () => process.exit(0));
  }
})().catch((e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
