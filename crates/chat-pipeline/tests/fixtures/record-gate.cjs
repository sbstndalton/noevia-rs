#!/usr/bin/env node
// Test-oracle glue only: drives noevia-core's own chat.cjs handleChat through one tool round (chat.cjs:2260-2560:
// the tool exchange, the permission gate, the approval card, the grant redeem and fence, the executor call and the
// tool_result event) with a synthetic model stream, synthetic policy/approval/grant/executor answers, and records
// what the turn sent and did. Needs dav-parse.wasm (tool-exchange checks): DAV_PARSE_WASM.
//   node record-gate.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), os = require('node:os'), cp = require('node:child_process');
const { EventEmitter } = require('node:events');
const core = path.resolve(process.argv[2]);
const S = (m) => require(path.join(core, 'server', m));
const { createChatHandler } = S('chat.cjs');
const { createToolExchange } = S('tool-exchange.cjs');
const { createVisionProbe } = S('vision.cjs');
const fixture = require(path.join(core, 'tests', 'server', 'chat-grant-fixture.cjs'));
let seed = 41;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const WRITES = new Set(['write_file', 'gdrive_update', 'delete_note']);
const TOOLS = ['web_search', 'read_file', 'write_file', 'gdrive_update', 'delete_note', 'unknown_tool'];
const ARGS = ['{"q":"x"}', '{"path":"/a.txt","text":"hi"}', '{}', '', '{bad', '[1]', '{"query":"Search the web for cats"}', '{"path":"/b.txt"}'];
const MODES = ['allow', 'ask', 'block'];
const DECISIONS = ['approve', 'approve', 'deny', 'timeout', 'aborted', 'unavailable', 'weird'];
const RESULTS = ['ok: done', 'ERROR: it failed', 'see https://a.example/x and https://b.example/y.', { throws: 'boom' }, { notRun: 'ERROR: unavailable' }, { failed: 'partial' }, 'x'.repeat(400)];
const REDEEM = ['fence-open', 'fence-open', 'fence-closed', 'unavailable', 'invalid'];
const TARGETS = [null, { target: '/drive/doc', kind: 'file' }, { target: '/drive/doc' }, { error: 'No such file' }, { throws: true }, { target: '' }];
const scenarios = [];
for (let i = 0; i < 2600; i++) {
  const n = 1 + rnd(3);
  const calls = Array.from({ length: n }, () => ({ name: pick(TOOLS), args: pick(ARGS) }));
  if (rnd(6) === 0 && n > 1) calls[n - 1] = { ...calls[0] }; // a repeated call: the exchange's cache
  scenarios.push({
    calls, modes: Object.fromEntries(TOOLS.map((t) => [t, pick(MODES)])), // The account's policy for one tool changes once that many approval cards were answered.
    flip: rnd(4) === 0 ? { tool: pick(TOOLS), afterDecisions: 1 + rnd(2), to: pick(MODES) } : null,
    open: rnd(10) !== 0, decisions: calls.map(() => pick(DECISIONS)), chatWide: rnd(3) === 0, redeem: pick(REDEEM), redeemWired: rnd(4) !== 0,
    results: calls.map(() => pick(RESULTS)), target: rnd(2) ? null : { first: pick(TARGETS), again: rnd(4) === 0 ? pick(TARGETS) : 'same' },
    grant: pick(['ok', 'ok', 'ok', 'tombstoned', 'moves']), chatId: rnd(8) ? 'fixture-chat' : null, preamble: pick(['', '', 'Let me check. ']),
  });
}
async function run(sc) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-gate-'));
  try {
    const events = [], res = new EventEmitter(), log = { exec: [], audit: [], opened: [], revoked: 0, toolMessages: null };
    res.writeHead = () => {}; res.setHeader = () => {}; res.write = (line) => events.push(JSON.parse(line.slice(6))); res.end = () => { res.writableEnded = true; res.emit('finish'); };
    let round = 0;
    const sse = (objs) => ({ ok: true, status: 200, headers: new Map(), body: (async function* () { for (const o of objs) yield Buffer.from(`data: ${JSON.stringify(o)}\n\n`); yield Buffer.from('data: [DONE]\n\n'); })() });
    const fetch = async (url, init) => {
      const body = JSON.parse(init.body);
      if (!body.stream) return { ok: true, status: 200, json: async () => ({ choices: [{ message: { content: 'Title' } }] }), text: async () => '{}' };
      const r = round++;
      if (r === 0) {
        const deltas = [];
        if (sc.preamble) deltas.push({ choices: [{ delta: { content: sc.preamble } }] });
        sc.calls.forEach((c, k) => deltas.push({ choices: [{ delta: { tool_calls: [{ index: k, id: `id-${k}`, function: { name: c.name, arguments: c.args } }] } }] }));
        deltas.push({ choices: [{ delta: {}, finish_reason: 'tool_calls' }] });
        return sse(deltas);
      }
      if (r === 1) log.toolMessages = body.messages.filter((m) => m.role === 'tool').map((m) => ({ id: m.tool_call_id, content: m.content }));
      return sse([{ choices: [{ delta: { content: 'Answer.' } }] }, { choices: [{ delta: {}, finish_reason: 'stop' }] }]);
    };
    const project = { id: 'fixture-project', name: 'Synthetic', model: 'answer-model', routing: 'manual', assets: [], files: [], chats: [{ id: 'fixture-chat' }] };
    const tools = TOOLS.map((name) => ({ type: 'function', function: { name, description: name, parameters: { type: 'object' } } }));
    let decisionAt = 0, resultAt = 0, targetCalls = 0, grantCalls = 0;
    const mode = (_user, name) => {
      if (sc.flip && sc.flip.tool === name && decisionAt >= sc.flip.afterDecisions) return sc.flip.to;
      return sc.modes[name];
    };
    const writeTargetFor = sc.target && (async () => {
      const t = targetCalls++ === 0 || sc.target.again === 'same' ? sc.target.first : sc.target.again;
      if (t && t.throws) throw new Error('lookup failed');
      return t;
    });
    const { handleChat } = createChatHandler({
      modelManager: { enabled: true, props: async () => ({ ok: false, status: 404 }), health: async () => ({ ok: true, body: { all_models_loaded: [{ model_name: 'answer-model', loaded: true, backend_alive: true, recipe_options: { ctx_size: 32768 } }] } }) },
      reasoningEffort: S('reasoning-effort.cjs'), authService: { audit: (event, _a, _b, meta) => log.audit.push([event, meta?.tool ?? null, meta?.reason ?? null]), diaryEnabled: () => false, getStorage: () => null },
      crypto: require('node:crypto'), path, fs, fetch, HISTORY_CAP: 20, DEFAULT_PROVIDER_ID: 'default', createToolExchange,
      currentWorkspace: () => ({ userId: 'synthetic-user', dir, assetDir: () => '/synthetic-only' }),
      getProject: () => project, getProvider: () => ({ id: 'default', baseUrl: 'http://default.invalid' }), providerHeaders: () => ({}),
      autoRoles: () => null, classifyFastOrSmart: async () => 'fast', servedCatalogue: async () => [], modelsInstalled: async () => [],
      missingRoles: () => [], staleRolesError: () => null, lastLoadedModel: () => null,
      skillsIndexFor: () => [], visionDescriptions: new Map(), visionProbe: createVisionProbe({ fetchImpl: fetch }),
      chatSkillRouter: { select: async () => ({ loaded: [] }) }, oauthServerIds: () => new Set(), accountReady: () => true, mcpOAuth: { connected: () => false },
      chatToolRouter: { select: async (ids) => ({ ids, routed: false }) }, DEFAULT_TOOLBOXES: ['core'], CONNECTOR_BOXES: new Set(['gdrive']), connectedBoxes: () => [],
      toolPolicy: { mode }, requestScope: { getStore: () => ({ workspace: { userId: 'synthetic-user' } }) },
      resolveTools: () => ({ tools, dropped: [], routes: [['gdrive_update', 'gdrive'], ['delete_note', 'notes']] }), isWriteTool: (n) => WRITES.has(n), allToolboxes: () => [{ id: 'core', tools }],
      rag: { filesContext: async () => null }, prefill: { recordSample() {} }, reduceToolResult: (text, { maxChars }) => ({ text: String(text).slice(0, Math.min(maxChars, 120)) }),
      diaryExtras: S('diary-extras.cjs'), DIARY_BASE: 'http://fixture.invalid', TOOL_RESULT_CAP: 8000, json: () => {}, saveChats() {}, endpointApproved: () => true, diaryHeaders: () => ({}),
      writeTargetFor,
      grantContext: async (raw) => { grantCalls++; return fixture.context(raw, { holder: { kind: 'project', id: 'fixture-project' }, tombstoned: sc.grant === 'tombstoned', generation: sc.grant === 'moves' ? String(grantCalls) : '0' }); },
      chatWideApproved: async () => sc.chatWide, recordUsage() {}, recordToolUse() {},
      openApproval: async ({ id }) => { log.opened.push(id.startsWith('ap-')); return sc.open; },
      awaitApproval: async () => sc.decisions[decisionAt++] ?? 'deny',
      ...(sc.redeemWired ? { redeemChatGrant: async () => {
        if (sc.redeem === 'unavailable') return 'unavailable';
        if (sc.redeem === 'invalid') return null;
        return { open: () => sc.redeem === 'fence-open', started: () => {} };
      } } : {}),
      revokeChatGrant: async () => { log.revoked++; },
      executeToolCall: async (_project, name, args, _allowed, _signal, outcome, options) => {
        const r = sc.results[resultAt++] ?? 'ok';
        log.exec.push({ name, args, fence: !!options.writeFence, routes: options.routes instanceof Map });
        if (typeof r === 'string') return r;
        if (r.throws) throw new Error(r.throws);
        if (r.notRun) { outcome.notRun = true; return r.notRun; }
        outcome.failed = true; return r.failed;
      },
    });
    const body = { projectId: 'fixture-project', message: 'Synthetic question', ...(sc.chatId ? { chatId: sc.chatId } : {}) };
    try { await handleChat({ headers: {} }, res, body, { user: { id: 'synthetic-user' } }); } catch (e) { log.thrown = `${e.constructor.name}: ${e.message}`; }
    const keep = new Set(['preamble', 'tool', 'tool_pending', 'tool_result', 'paused']);
    return { events: events.filter((e) => keep.has(e.type)).map(({ id, ...e }) => (e.type === 'tool_pending' ? { ...e, idShape: /^ap-[0-9a-f-]{36}$/.test(id) } : e)), ...log };
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
}
(async () => {
  const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), writes: [...WRITES], cases: [] };
  const log = console.log; console.log = () => {}; console.warn = () => {};
  for (const sc of scenarios) out.cases.push({ ...sc, out: await run(sc) });
  console.log = log;
  fs.writeFileSync(process.argv[3], JSON.stringify(out));
  const kinds = {};
  for (const c of out.cases) for (const e of c.out.events.filter((e) => e.type === 'tool_result')) { const k = e.text.slice(0, 48); kinds[k] = (kinds[k] || 0) + 1; }
  console.log(JSON.stringify(kinds, null, 1));
  console.log('thrown', out.cases.filter((c) => c.out.thrown).length, 'noToolMsgs', out.cases.filter((c) => !c.out.toolMessages).length);
})();
