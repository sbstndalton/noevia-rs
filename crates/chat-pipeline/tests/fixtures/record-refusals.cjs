#!/usr/bin/env node
// Test-oracle glue only: drives noevia-core's own chat.cjs handleChat through the routing and provider checks
// (chat.cjs:900-1150: routing modes, egress, ChatGPT, Auto roles, model choice) with synthetic projects, providers and
// catalogues, and records the first answer: a refusal {status, payload}, or the turn going on ({model, provider, events}).
// Needs dav-parse.wasm (provider egress is Rust-only): DAV_PARSE_WASM.
//   node record-refusals.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), os = require('node:os'), cp = require('node:child_process');
const { EventEmitter } = require('node:events');
const core = path.resolve(process.argv[2]);
const S = (m) => require(path.join(core, 'server', m));
const { createChatHandler } = S('chat.cjs');
const { createToolExchange } = S('tool-exchange.cjs');
const { createVisionProbe } = S('vision.cjs');
const { missingRoles, staleRolesError } = S('auto-roles-check.cjs');
let seed = 23;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const PROVIDERS = {
  default: { id: 'default', baseUrl: 'http://default.invalid' },
  ext: { id: 'ext', kind: 'openai', external: true, label: 'Cloud Synth', baseUrl: 'https://cloud.invalid/v1' },
  extnolabel: { id: 'extnolabel', kind: 'openai', external: true, baseUrl: 'https://cloud2.invalid/v1' },
  lan: { id: 'lan', label: 'LAN box', baseUrl: 'http://10.0.0.9:8080/v1' },
  chatgpt: { id: 'chatgpt', kind: 'chatgpt-oauth', label: 'ChatGPT', baseUrl: 'https://chatgpt.invalid' },
  chatgptShared: { id: 'chatgptShared', kind: 'chatgpt-oauth', label: 'ChatGPT', shared: true, baseUrl: 'https://chatgpt.invalid' },
};
const ROLES = [null, { fast: 'fast-m', smart: 'smart-m' }, { fast: 'fast-m' }, { smart: 'smart-m' }, { fast: 'fast-m', smart: 'smart-m', code: 'code-m' }, { fast: 'fast-m', smart: 'smart-m', creative: 'creative-m' }];
const CATS = [[], [{ name: 'fast-m' }, { name: 'smart-m' }], [{ name: 'fast-m' }, { name: 'smart-m', missingFile: true }], [{ name: 'fast-m' }, { name: 'smart-m' }, { name: 'code-m' }, { name: 'creative-m' }],
  [{ name: 'answer-m' }, { name: 'embed-x' }, { name: 'vec-m', labels: ['embedding'] }, { name: 'rr-m', labels: ['Reranking'] }, { name: 'fast-m' }, { name: 'smart-m' }], null];
const PROJECT_MODELS = [undefined, null, '', 'answer-m', 'embed-x', 'vec-m', 'rr-m', 'unlisted-m', 'Laya-1', 'fast-m'];
const SETTINGS = [null, { mode: 'cloud', cloud: {} }, { mode: 'cloud', cloud: { providerId: 'ext' } }, { mode: 'cloud', cloud: { providerId: 'ext', smart: 'c-smart' } },
  { mode: 'cloud', cloud: { providerId: 'ext', fast: 'c-fast' } }, { mode: 'local', cloud: { providerId: 'ext', smart: 'c-smart' } },
  { mode: 'hybrid', whenSensitive: 'ask', cloud: { providerId: 'ext', smart: 'c-smart' } }, { mode: 'hybrid', whenSensitive: 'local', cloud: { providerId: 'ext', smart: 'c-smart' } },
  { mode: 'cloud', cloud: { providerId: 'default', smart: 'c-smart' } }];
const VERDICTS = [{ flagged: false }, { flagged: true, flag: 'health' }, { flagged: true, flag: 'unavailable', cause: 'timeout' }, { flagged: true, flag: 'unavailable', cause: '' }, { flagged: true, flag: 'unavailable' }, { flagged: true, cause: 'deadline' }, null];
const ANSWERS = [{ choice: 'cloud' }, { choice: 'cloud', remember: true }, { choice: 'local' }, { choice: 'local', remember: true }, { choice: 'timeout' }];
const scenarios = [];
for (let i = 0; i < 2500; i++) {
  scenarios.push({
    space: pick(['free', 'free', 'project', 'diaryProject']), routing: pick(['auto', 'manual', undefined]), providerId: pick([undefined, 'default', 'lemonade', 'ext', 'extnolabel', 'lan', 'chatgpt', 'chatgptShared']),
    model: pick(PROJECT_MODELS), roles: pick(ROLES), rolesGone: rnd(12) === 0, catalogue: pick(CATS), loaded: pick([null, 'loaded-m']), managerOn: rnd(2) === 1, role: pick(['fast', 'smart', 'code']),
    rm: rnd(3) === 0 ? null : { settings: rnd(3) === 0 ? pick(SETTINGS.slice(6, 8)) : pick(SETTINGS), verdict: pick(VERDICTS), answer: pick(ANSWERS) }, chatFlags: pick([{}, { forceLocal: true }, { allowCloud: true }]),
    chatgpt: pick([null, { enabled: true, state: 'connected' }, { enabled: false, state: 'connected' }, { enabled: true, state: 'reconnect' }, { enabled: true, state: 'disconnected' }]),
    owner: pick(['u-synth', 'u-synth', null]), workspaceUser: pick(['u-synth', 'u-synth', 'u-other']), message: pick(['Synthetic question', 'my blood test results', 'hi', 'Write a short poem about rain', 'my IBAN is DE89370400440532013000']),
  });
}
// The sensitive-content card (route_pending, route_remembered) and a refusal after it (an error event).
for (let i = 0; i < 400; i++) {
  scenarios.push({ space: pick(['free', 'project']), routing: 'auto', providerId: pick([undefined, 'default']), model: undefined, roles: pick(ROLES.slice(1)), rolesGone: false,
    catalogue: pick(CATS), loaded: null, managerOn: false, role: pick(['fast', 'smart', 'code']),
    rm: { settings: { mode: 'hybrid', whenSensitive: pick(['ask', 'ask', undefined, 'local']), cloud: { providerId: pick(['ext', 'ext', 'lan']), smart: pick(['c-smart', '']), fast: pick([undefined, 'c-fast']) } }, verdict: pick(VERDICTS), answer: pick(ANSWERS) },
    chatFlags: pick([{}, {}, { forceLocal: true }, { allowCloud: true }]), chatgpt: null, owner: 'u-synth', workspaceUser: pick(['u-synth', 'u-synth', null]),
    message: pick(['my blood test results', 'my IBAN is DE89370400440532013000', 'hi', 'Write a short poem about rain']) });
}
const DONE = Symbol('upstream');
async function run(sc) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-refusals-'));
  try {
    const events = [], res = new EventEmitter();
    res.writeHead = () => {}; res.setHeader = () => {}; res.write = (line) => events.push(JSON.parse(line.slice(6))); res.end = () => { res.writableEnded = true; res.emit('finish'); };
    let answer = null, upstream = null, autoCalls = 0;
    const fetch = async (url, init) => { upstream = { url: String(url), model: JSON.parse(init.body).model ?? null }; throw DONE; };
    const DIARY_PROJECT = S('diary-extras.cjs').PROJECT_ID;
    const project = sc.space === 'diary' ? null : sc.space === 'diaryProject' ? { id: DIARY_PROJECT, name: 'Synthetic', routing: sc.routing, model: sc.model, provider: sc.providerId, assets: [], files: [], chats: [{ id: 'fixture-chat', ...sc.chatFlags }] } : sc.space === 'project' || sc.routing !== undefined || sc.model !== undefined || sc.providerId !== undefined
      ? { id: 'fixture-project', name: 'Synthetic', routing: sc.routing, model: sc.model, provider: sc.providerId, assets: [], files: [], chats: [{ id: 'fixture-chat', ...sc.chatFlags }] } : null;
    const routingModes = sc.rm && { enabled: () => true, settings: () => sc.rm.settings, sensitivity: async () => sc.rm.verdict, chunkChars: () => 1000, maxChunks: () => 4,
      openChoice: async () => true, awaitChoice: async () => sc.rm.answer, log() {}, setChatFlags() {} };
    const chatgptOAuth = sc.chatgpt && { status: () => ({ state: sc.chatgpt.state }), fetchFor: () => fetch };
    const { handleChat } = createChatHandler({
      modelManager: { enabled: sc.managerOn, load: async (m) => { upstream = { load: String(m) }; throw DONE; }, props: async () => ({ ok: false, status: 404 }), health: async () => ({ ok: true, body: { all_models_loaded: [] } }) },
      reasoningEffort: S('reasoning-effort.cjs'), authService: { audit() {}, diaryEnabled: () => true, getStorage: () => null }, crypto: require('node:crypto'), path, fs, fetch,
      HISTORY_CAP: 20, DEFAULT_PROVIDER_ID: 'default', createToolExchange,
      currentWorkspace: () => ({ userId: sc.workspaceUser, dir, assetDir: () => '/synthetic-only' }),
      getProject: (id) => (project && id === project.id ? project : null), getProvider: (id) => PROVIDERS[id] || null, providerHeaders: () => ({}),
      // rolesGone: the saved roles vanish between the Auto check and the role read (a concurrent save).
      autoRoles: () => { autoCalls++; return sc.rolesGone && autoCalls > 1 ? null : sc.roles; }, classifyFastOrSmart: async () => sc.role, servedCatalogue: async () => sc.catalogue, modelsInstalled: async () => sc.catalogue || [],
      missingRoles, staleRolesError, lastLoadedModel: () => sc.loaded,
      skillsIndexFor: () => [], visionDescriptions: new Map(), visionProbe: createVisionProbe({ fetchImpl: fetch }),
      chatSkillRouter: { select: async () => ({ loaded: [] }) }, oauthServerIds: () => new Set(), accountReady: () => true, mcpOAuth: { connected: () => false },
      chatToolRouter: { select: async (ids) => ({ ids, routed: false }) }, DEFAULT_TOOLBOXES: ['core'], CONNECTOR_BOXES: new Set(['gdrive']), connectedBoxes: () => [],
      toolPolicy: { mode: () => 'allow' }, requestScope: { getStore: () => ({ workspace: { userId: sc.workspaceUser } }) },
      resolveTools: () => ({ tools: [], dropped: [], routes: [] }), isWriteTool: () => false, allToolboxes: () => [],
      rag: { filesContext: async () => null }, prefill: { recordSample() {} }, reduceToolResult: () => ({ text: 'reduced' }), diaryExtras: S('diary-extras.cjs'),
      DIARY_BASE: 'http://fixture.invalid', TOOL_RESULT_CAP: 8000, json: (_res, status, payload) => { answer = answer || { status, payload }; }, saveChats() {}, endpointApproved: () => true, diaryHeaders: () => ({}),
      routingModes, freeChats: () => [{ id: 'fixture-chat', ...sc.chatFlags }], chatgptOAuth, chatgptEnabled: () => !!sc.chatgpt?.enabled,
      grantContext: async (raw) => require(path.join(core, 'tests', 'server', 'chat-grant-fixture.cjs')).context(raw, { holder: { kind: 'absent' } }),
      chatWideApproved: () => false, recordUsage() {}, recordToolUse() {}, executeToolCall: async () => 'unused', awaitApproval: async () => 'decline',
    });
    const body = { chatId: 'fixture-chat', message: sc.message, ...(sc.space === 'diary' ? { spaceId: 'diary' } : project ? { projectId: project.id } : {}) };
    const authn = sc.owner ? { user: { id: sc.owner } } : null;
    let thrown = null;
    try { await handleChat({ headers: {} }, res, body, authn); } catch (e) { if (e !== DONE) thrown = `${e.constructor.name}: ${e.message} @ ${(e.stack.split("\n")[1] || "").trim()}`; }
    const routeEvents = events.filter((e) => e.type === 'route_pending' || e.type === 'route_remembered').map(({ id: _id, ...e }) => e);
    if (answer) return { refused: answer, events: routeEvents };
    if (upstream) return { upstream, events: routeEvents, routing: events.find((e) => e.type === 'meta')?.routing ?? null };
    return { other: thrown || events.map((e) => e.type === 'error' ? `error:${e.code || ''}:${e.text}` : e.type).join(',') || 'none', events: routeEvents };
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
}
(async () => {
  const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), providers: PROVIDERS, diaryProjectId: S('diary-extras.cjs').PROJECT_ID, cases: [] };
  const log = console.log; console.log = () => {}; console.warn = () => {};
  const sampling = S('sampling-presets.cjs'), rm = S('routing-modes.cjs');
  // The verdicts of core's own classifiers (ported separately) that the stage takes as inputs.
  for (const sc of scenarios) {
    const verdicts = { preFlag: rm.preRule(sc.message), creative: Object.fromEntries(['fast', 'smart', 'code'].map((r) => [r, sampling.classifyTaskForSampling({ routedRole: r, message: sc.message }) === 'creative'])) };
    out.cases.push({ ...sc, verdicts, out: await run(sc) });
  }
  console.log = log;
  fs.writeFileSync(process.argv[3], JSON.stringify(out));
  const kinds = {};
  for (const c of out.cases) { const o = c.out; const k = o.refused ? `${o.refused.status} ${o.refused.payload.error.slice(0, 50)}` : o.upstream ? 'upstream' : `other ${o.other}`; kinds[k] = (kinds[k] || 0) + 1; }
  console.log(JSON.stringify(kinds, null, 1));
})();
