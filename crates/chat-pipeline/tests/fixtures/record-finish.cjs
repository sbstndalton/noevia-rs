#!/usr/bin/env node
// Test-oracle glue only: drives noevia-core's own chat.cjs handleChat to the end of a turn (chat.cjs:2620-2670: the
// `done` event, whether the chat's title is asked for (#1785: first completed reply, auto-named, local or machine
// provider, not Diary, not compact, engine idle, at most 3 attempts per chat), the `title` event and its save), turn
// after turn on one handler, with synthetic answers. Records each turn's events from `telemetry` on and the title
// requests and saves. Synthetic data only.
//   node record-finish.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), os = require('node:os'), cp = require('node:child_process');
const { EventEmitter } = require('node:events');
const core = path.resolve(process.argv[2]);
const S = (m) => require(path.join(core, 'server', m));
const { createChatHandler } = S('chat.cjs');
const { createToolExchange } = S('tool-exchange.cjs');
const { createVisionProbe } = S('vision.cjs');
let seed = 59;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const PROVIDERS = { default: { id: 'default', baseUrl: 'http://default.invalid' }, lan: { id: 'lan', label: 'LAN', baseUrl: 'http://10.0.0.9:8080' },
  ext: { id: 'ext', kind: 'openai', external: true, label: 'Cloud', baseUrl: 'https://cloud.invalid' } };
const HISTORIES = [[], [{ role: 'user', content: 'First question' }], [{ role: 'user', content: 'First question' }, { role: 'assistant', content: 'An answer' }], [{ role: 'assistant', content: 'Hello' }]];
const scenarios = [];
for (let i = 0; i < 700; i++) {
  scenarios.push({
    turns: Array.from({ length: 1 + rnd(4) }, () => ({ answer: pick(['A full answer.', 'A full answer.', '', '   ']), title: pick(['Train Times', '<think>x</think>Rain', '', null, 'error']),
      busy: rnd(5) === 0, autoNamed: pick([true, false, 'throws', undefined]), history: pick(HISTORIES), chatId: pick(['c1', 'c1', 'c2', '', undefined, 7]) })),
    titles: rnd(6) !== 0, provider: pick(['default', 'default', 'lan', 'ext']), compactOnly: rnd(8) === 0, project: rnd(3) === 0, message: pick(['How long is the train ride?', '  ', 'x']),
  });
}
async function run(sc) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-finish-'));
  try {
    const log = { turns: [] };
    let turn = null, cur = null;
    const sse = (objs) => ({ ok: true, status: 200, headers: new Map(), body: (async function* () { for (const o of objs) yield Buffer.from(`data: ${JSON.stringify(o)}\n\n`); yield Buffer.from('data: [DONE]\n\n'); })() });
    const fetch = async (url, init) => {
      const body = JSON.parse(init.body);
      if (!body.stream) {
        turn.titleAsked = { url: String(url), model: body.model, temperature: body.temperature };
        if (cur.title === 'error') return { ok: false, status: 500, json: async () => ({}) };
        return { ok: true, status: 200, json: async () => ({ choices: [{ message: { content: cur.title } }] }), headers: new Map([['content-type', 'application/json']]),
          text: async () => JSON.stringify({ choices: [{ message: { content: cur.title } }] }) };
      }
      return sse(cur.answer ? [{ choices: [{ delta: { content: cur.answer } }] }, { choices: [{ delta: {}, finish_reason: 'stop' }] }] : [{ choices: [{ delta: {}, finish_reason: 'stop' }] }]);
    };
    const project = sc.project ? { id: 'fixture-project', name: 'Synthetic', model: 'answer-model', routing: 'manual', provider: sc.provider, assets: [], files: [] } : null;
    const chatTitles = sc.titles ? {
      autoNamed: (list, chat, first) => { turn.autoNamedAsked = [list, chat, first]; if (cur.autoNamed === 'throws') throw new Error('x'); return cur.autoNamed; },
      save: (list, chat, title, first) => { turn.saved = [list, chat, title, first]; },
    } : null;
    const { handleChat } = createChatHandler({
      modelManager: { enabled: true, inferenceBusy: () => cur.busy, props: async () => ({ ok: false, status: 404 }), health: async () => ({ ok: true, body: { all_models_loaded: [{ model_name: 'answer-model', loaded: true, backend_alive: true, recipe_options: { ctx_size: 32768 } }] } }) },
      reasoningEffort: S('reasoning-effort.cjs'), authService: { audit() {}, diaryEnabled: () => false, getStorage: () => null }, crypto: require('node:crypto'), path, fs, fetch,
      HISTORY_CAP: 20, DEFAULT_PROVIDER_ID: 'default', createToolExchange,
      currentWorkspace: () => ({ userId: 'synthetic-user', dir, assetDir: () => '/synthetic-only' }),
      getProject: () => project, getProvider: (id) => PROVIDERS[id] || PROVIDERS.default, providerHeaders: () => ({}),
      autoRoles: () => null, classifyFastOrSmart: async () => 'fast', servedCatalogue: async () => [], modelsInstalled: async () => [],
      missingRoles: () => [], staleRolesError: () => null, lastLoadedModel: () => 'answer-model',
      skillsIndexFor: () => [], visionDescriptions: new Map(), visionProbe: createVisionProbe({ fetchImpl: fetch }),
      chatSkillRouter: { select: async () => ({ loaded: [] }) }, oauthServerIds: () => new Set(), accountReady: () => true, mcpOAuth: { connected: () => false },
      chatToolRouter: { select: async (ids) => ({ ids, routed: false }) }, DEFAULT_TOOLBOXES: ['core'], CONNECTOR_BOXES: new Set(['gdrive']), connectedBoxes: () => [],
      toolPolicy: { mode: () => 'allow' }, requestScope: { getStore: () => ({ workspace: { userId: 'synthetic-user' } }) },
      resolveTools: () => ({ tools: [], dropped: [], routes: [] }), isWriteTool: () => false, allToolboxes: () => [],
      rag: { filesContext: async () => null }, prefill: { recordSample() {} }, reduceToolResult: (t) => ({ text: String(t) }), diaryExtras: S('diary-extras.cjs'),
      DIARY_BASE: 'http://fixture.invalid', TOOL_RESULT_CAP: 8000, json: (_r, status, payload) => { turn.json = { status, payload }; }, saveChats() {}, endpointApproved: () => true, diaryHeaders: () => ({}),
      chatTitles, grantContext: async (raw) => require(path.join(core, 'tests', 'server', 'chat-grant-fixture.cjs')).context(raw, { holder: { kind: 'absent' } }),
      chatWideApproved: () => false, recordUsage() {}, recordToolUse() {}, executeToolCall: async () => 'unused', awaitApproval: async () => 'decline',
    });
    for (cur of sc.turns) {
      turn = { titleAsked: null, autoNamedAsked: null, saved: null };
      const events = [], res = new EventEmitter();
      res.writeHead = () => {}; res.setHeader = () => {}; res.write = (line) => events.push(JSON.parse(line.slice(6))); res.end = () => { res.writableEnded = true; res.emit('finish'); };
      const body = { message: sc.message, history: cur.history, ...(cur.chatId !== undefined ? { chatId: cur.chatId } : {}), ...(project ? { projectId: project.id } : {}), ...(sc.compactOnly ? { compactOnly: true } : {}) };
      try { await handleChat({ headers: {} }, res, body, { user: { id: 'synthetic-user' } }); } catch (e) { turn.thrown = String(e.message); }
      const from = events.findIndex((e) => e.type === 'telemetry' && e.phase === 'complete');
      turn.tail = from < 0 ? events.map((e) => e.type) : events.slice(from).map((e) => (e.type === 'telemetry' ? { type: 'telemetry', model: e.model } : e));
      log.turns.push(turn);
    }
    return log;
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
}
(async () => {
  const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), providers: PROVIDERS, cases: [] };
  const log = console.log; console.log = () => {}; console.warn = () => {};
  for (const sc of scenarios) out.cases.push({ ...sc, out: await run(sc) });
  console.log = log;
  fs.writeFileSync(process.argv[3], JSON.stringify(out));
  const k = {};
  for (const c of out.cases) for (const t of c.out.turns) { const key = `${t.titleAsked ? 'asked' : 'not'} ${t.tail.map((e) => e.type || e).join(',')}`; k[key] = (k[key] || 0) + 1; }
  console.log(JSON.stringify(k, null, 1));
})();
