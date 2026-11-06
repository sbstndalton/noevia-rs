#!/usr/bin/env node
// Test-oracle glue only: drives noevia-core's own chat.cjs handleChat, with core's own toolboxes.cjs catalogue
// (createToolboxes: resolveTools, the count cap, the token budget, isWriteTool), through the tool selection of a turn
// (requestToolboxes, Automatic additions and the carry, the Diary/sign-in/external-provider filters, the toolbox
// router's answer, more_tools) and records what the first model request offers and the tools_scope event. Also
// records toolboxes.cjs estimateToolTokens/toolCapFor/toolTokenBudgetFor directly. Synthetic boxes and messages only.
// Needs dav-parse.wasm: DAV_PARSE_WASM.
//   node record-select.cjs <noevia-core checkout> <output.json>
'use strict';
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
const IDS = ['core', ...BOXES.map((b) => b.id), ...MCP.map((b) => b.id), 'unknown-box'];
const MESSAGES = ['hi', 'what is the latest news', 'search the web for cats', 'my notes from monday', 'list my nextcloud files', 'wikipedia on rust',
  'my notes and search the web', 'send my mail', 'look it up online please', 'my files and wikipedia and the latest news'];
const HIST = [[], [{ role: 'user', content: 'search the web for cats' }, { role: 'assistant', content: 'found' }], [{ role: 'user', content: 'my notes' }, { role: 'assistant', content: 'ok' }],
  [{ role: 'user', content: 'x' }, { role: 'tool', name: 'web_search', content: 'r' }, { role: 'assistant', content: 'y' }]];
const MODELS = ['synth-9B-q4', 'synth-70b', 'synth 1.5 b', 'plain-model', 'synth-12B', 'synth-13b'];
const PROVIDERS = { default: { id: 'default', baseUrl: 'http://default.invalid' }, ext: { id: 'ext', kind: 'openai', external: true, label: 'Cloud', baseUrl: 'https://cloud.invalid/v1' },
  lan: { id: 'lan', label: 'LAN', baseUrl: 'http://10.0.0.9:8080/v1' } };
const scenarios = [];
for (let i = 0; i < 1600; i++) {
  const sc = {
    project: rnd(8) === 0 ? null : { toolsMode: pick(['auto', 'manual', undefined]), toolboxes: rnd(4) === 0 ? undefined : subset(IDS), docsToolboxDefaulted: rnd(3) === 0, files: rnd(3) === 0 ? [{ name: 'a.txt' }] : [] },
    providerId: pick(['default', 'default', 'ext', 'lan']), model: pick(MODELS), message: pick(MESSAGES), history: pick(HIST),
    turnToolboxes: rnd(3) === 0 ? subset(IDS).concat(rnd(4) === 0 ? [3] : []) : undefined, disabled: subset(IDS).filter(() => rnd(4) === 0),
    connected: rnd(3) === 0 ? ['gdrive'] : [], diaryEnabled: rnd(3) > 0, oauth: subset(['nextcloud-notes', 'ext-mcp']), notSignedIn: subset(['nextcloud-notes', 'ext-mcp']),
    notReady: subset(['nextcloud-notes', 'nextcloud-files', 'web-search']).filter(() => rnd(2)), blockedNames: subset(['web_search', 'nc_notes_write', 'big_3', 'read_project_file', 'huge_read']).filter(() => rnd(2)),
    measured: pick([null, null, 0, 900, 3000, 20000, 6400.5]), router: pick(['off', 'off', 'narrow', 'all', 'reverse']),
  };
  // A free chat answers on this machine's Auto roles.
  if (!sc.project) sc.providerId = 'default';
  scenarios.push(sc);
}
const DONE = Symbol('upstream');
async function run(sc) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-select-'));
  try {
    const events = [], res = new EventEmitter();
    res.writeHead = () => {}; res.setHeader = () => {}; res.write = (line) => events.push(JSON.parse(line.slice(6))); res.end = () => { res.writableEnded = true; res.emit('finish'); };
    let answer = null, upstream = null, routerCall = null;
    const fetch = async (url, init) => { const b = JSON.parse(init.body); upstream = { model: b.model ?? null, names: b.tools ? b.tools.map((t) => t.function.name) : null,
      toolsSha: b.tools ? require('node:crypto').createHash('sha256').update(JSON.stringify(b.tools)).digest('hex') : null }; throw DONE; };
    const toolboxes = tb.createToolboxes({ boxes: BOXES, mcpBoxes: () => MCP, offered: (id) => !sc.disabled.includes(id),
      prefill: { budgetFor: () => sc.measured, rateFor: () => 0.3 } });
    const project = sc.project && { id: 'fixture-project', name: 'Synthetic', routing: 'manual', model: sc.model, provider: sc.providerId, assets: [], chats: [{ id: 'fixture-chat' }], ...sc.project };
    const chatToolRouter = { select: async (ids, message) => {
      const answer = sc.router === 'off' || ids.length <= 1 ? { ids, routed: false, reason: 'off' }
        : sc.router === 'narrow' ? { ids: ids.slice(1, 3), routed: true, narrowed: ids.slice(1, 3).length < ids.length }
          : sc.router === 'all' ? { ids: [...ids], routed: true, narrowed: false } : { ids: [...ids].reverse().slice(0, Math.max(1, ids.length - 1)), routed: true, narrowed: true };
      routerCall = { ids: [...ids], message, answer }; return answer; } };
    const { handleChat } = createChatHandler({
      modelManager: { enabled: false }, reasoningEffort: S('reasoning-effort.cjs'), authService: { audit() {}, diaryEnabled: () => sc.diaryEnabled, getStorage: () => null },
      crypto: require('node:crypto'), path, fs, fetch, HISTORY_CAP: 20, DEFAULT_PROVIDER_ID: 'default', createToolExchange,
      currentWorkspace: () => ({ userId: 'u-synth', dir, assetDir: () => '/synthetic-only' }),
      getProject: (id) => (project && id === project.id ? project : null), getProvider: (id) => PROVIDERS[id] || null, providerHeaders: () => ({}),
      autoRoles: () => (project ? null : { fast: sc.model, smart: sc.model }), classifyFastOrSmart: async () => 'fast', servedCatalogue: async () => [{ name: sc.model }], modelsInstalled: async () => [{ name: sc.model }],
      missingRoles, staleRolesError, lastLoadedModel: () => sc.model, skillsIndexFor: () => [], visionDescriptions: new Map(), visionProbe: createVisionProbe({ fetchImpl: fetch }),
      chatSkillRouter: { select: async () => ({ loaded: [] }) }, oauthServerIds: () => new Set(sc.oauth), accountReady: (_u, id) => !sc.notSignedIn.includes(id), mcpOAuth: { connected: () => false },
      automaticToolsReady: () => (box) => !sc.notReady.includes(box.id),
      chatToolRouter, DEFAULT_TOOLBOXES: toolboxes.DEFAULT_TOOLBOXES, CONNECTOR_BOXES: toolboxes.CONNECTOR_BOXES, connectedBoxes: () => sc.connected,
      toolPolicy: { mode: (_u, name) => (sc.blockedNames.includes(name) ? 'block' : 'allow') }, requestScope: { getStore: () => ({ workspace: { userId: 'u-synth' }, authn: { user: { id: 'u-synth' } } }) },
      resolveTools: toolboxes.resolveTools, isWriteTool: toolboxes.isWriteTool, allToolboxes: toolboxes.allToolboxes,
      rag: { filesContext: async () => null }, prefill: { recordSample() {} }, reduceToolResult: () => ({ text: 'reduced' }), diaryExtras: S('diary-extras.cjs'),
      DIARY_BASE: 'http://fixture.invalid', TOOL_RESULT_CAP: 8000, json: (_res, status, payload) => { answer = answer || { status, payload }; }, saveChats() {}, endpointApproved: () => true, diaryHeaders: () => ({}),
      freeChats: () => [{ id: 'fixture-chat' }],
      grantContext: async (raw) => require(path.join(core, 'tests', 'server', 'chat-grant-fixture.cjs')).context(raw, { holder: { kind: 'absent' } }),
      chatWideApproved: () => false, recordUsage() {}, recordToolUse() {}, executeToolCall: async () => 'unused', awaitApproval: async () => 'decline',
    });
    const body = { chatId: 'fixture-chat', message: sc.message, history: sc.history, ...(project ? { projectId: project.id } : {}),
      ...(sc.turnToolboxes ? { turnToolboxes: sc.turnToolboxes } : {}) };
    let thrown = null;
    try { await handleChat({ headers: {} }, res, body, { user: { id: 'u-synth' } }); } catch (e) { if (e !== DONE) thrown = `${e.constructor.name}: ${e.message}`; }
    const all = toolboxes.allToolboxes();
    const names = [...new Set(all.flatMap((b) => (b.tools || []).map((t) => t?.function?.name)).filter(Boolean)), 'read_project_file', 'get_current_time', 'more_tools'];
    return {
      boxes: all.map((b) => b.id), writes: names.filter((n) => toolboxes.isWriteTool(n)), externalStrict: egress.stripPrivateToolboxes(['diary'], PROVIDERS[sc.providerId]).length > 0,
      routerCall, scope: events.find((e) => e.type === 'tools_scope') || null, upstream, refused: answer, thrown,
      other: upstream || answer || thrown ? undefined : events.map((e) => (e.type === 'error' ? `error:${e.code || ''}:${e.text}` : e.type)).join(','),
    };
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
}
(async () => {
  const ALL = tb.createToolboxes({ boxes: BOXES, mcpBoxes: () => MCP, prefill: { budgetFor: () => null, rateFor: () => 0.3 } }).allToolboxes();
  const out = { ALL, core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), units: [], cases: [] };
  const log = console.log, warn = console.warn; console.log = () => {}; console.warn = () => {};
  for (const m of [...MODELS, '', '7B', '12.5b', 'a12bx', 'x 3 B!', 'v2-0.5B', '1e3b', '99 b c 3b']) {
    const t = tb.createToolboxes({ prefill: { budgetFor: () => null, rateFor: () => 0.3 } }), t2 = tb.createToolboxes({ prefill: { budgetFor: () => 777, rateFor: () => 0.3 } });
    out.units.push({ model: m, cap: tb.toolCapFor(m), budget: t.toolTokenBudgetFor(m), measured777: t2.toolTokenBudgetFor(m) });
  }
  for (let k = 0; k < 60; k++) { const tools = subset([...BOXES, ...MCP].flatMap((b) => b.tools)); out.units.push({ tools, est: tb.estimateToolTokens(tools) }); }
  for (const sc of scenarios) out.cases.push({ ...sc, out: await run(sc) });
  console.log = log; console.warn = warn;
  fs.writeFileSync(process.argv[3], JSON.stringify(out));
  const kinds = {};
  for (const c of out.cases) { const o = c.out; const k = o.thrown ? `thrown ${o.thrown}` : o.other ? `other ${o.other.slice(0, 120)}` : o.refused ? `refused ${o.refused.status} ${String(o.refused.payload.error).slice(0, 60)}` : o.upstream ? `upstream tools=${o.upstream.names ? 'y' : 'n'} scope=${o.scope?.boxes ? 'y' : 'n'}` : 'none'; kinds[k] = (kinds[k] || 0) + 1; }
  console.log(JSON.stringify(kinds, null, 1));
})();
