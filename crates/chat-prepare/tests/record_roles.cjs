#!/usr/bin/env node
'use strict';
// Recorded routing of a turn's answering model and role for crates/chat-prepare (slice P5b): the contract src/roles.rs,
// src/router.rs and src/system_one.rs are replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-prepare/tests/record_roles.cjs > crates/chat-prepare/tests/fixtures/node-roles.v1.json
//   ... node crates/chat-prepare/tests/record_roles.cjs --check crates/chat-prepare/tests/fixtures/node-roles.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Nothing from node_modules and no dav-parse.wasm. Every expectation comes from noevia-core's own code; the inputs are
// synthetic and QA-named (hand-made edges plus a seeded PRNG), passed through JSON first so Node reads exactly what is
// recorded (a field left out of a case is `undefined`):
//   heuristics  auto-router.cjs heuristicWantsSmart and heuristicWantsCode, system-one-router.cjs clearlySimple
//   verdicts    classifierVerdict(msg, withCode) (or `thrown`), bodies: classifierBody(model, message, suppress, withCode)
//   classify    createAutoRouter(...).classify(message) over stub roles, admission and fetchJson answers (`'throw'`: the
//               stub throws); `sent` holds the request bodies
//   systemOne   createSystemOneRouter(...).classifyWithDetails(message) over stub roles, feature switch, backend and
//               decision result (decision/index.cjs createDecisions is replaced by a stub that records the request);
//               `fallbacks` counts the calls of the auto router
//   missing     auto-roles-check.cjs missingRoles(roles, installed) and staleRolesError of it; creative: creativeMoveIsFree
//   chatModels  chat-model-kind.cjs isChatGenerationModel(name, labels) under model-system.cjs's section roles
//   turns       chat.cjs's own lines (cut out of the pinned chat.cjs, run as they are): projectProvider and wantsAuto, the
//               provider line, recordResend, and the block from creativeMoveAllowed to the chat-model guard, over stubs
//               (no routing modes, no Inference machines). `calls` lists the seams in the order they ran (autoRoles,
//               getProvider, servedCatalogue, codeProject, startPreload, classify, recordOutcome, modelsInstalled,
//               lastLoadedModel) and what was done with a fake pre-load (use, cancel, and releaseIfUnused when the
//               response ends).
// Each source file's sha256 is recorded, so a changed Node file needs a fresh recording. Node's console output goes to
// stderr; stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => process.stderr.write(`${a.map(String).join(' ')}\n`);
const READ_FILES = ['server/chat.cjs', 'server/index.cjs'];
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
for (const wiring of ['const classifyFastOrSmart = (message) => systemOneRouter.classifyWithDetails(message);',
  '  fallback: message => autoRouter.classify(message),\n});', '  roles: () => autoRoles(),\n  provider: () => getProvider(DEFAULT_PROVIDER_ID),',
  "const { missingRoles, staleRolesError } = require('./auto-roles-check.cjs');"]) {
  if (!indexText.includes(wiring)) throw new Error(`index.cjs no longer holds ${JSON.stringify(wiring.slice(0, 60))}`);
}
// The decision service is a seam: its result is the case's, and the request is recorded.
let decideCase = null;
const decisionIndex = serverRequire('decision/index.cjs');
decisionIndex.createDecisions = () => ({ decide: async (request) => { decideCase.request = JSON.parse(JSON.stringify(request)); return decideCase.result; } });
const { createAutoRouter } = serverRequire('auto-router.cjs');
const { createSystemOneRouter, clearlySimple } = serverRequire('system-one-router.cjs');
const { DECISION_SERVICE_REPORTED_MODEL } = serverRequire('decision-models.cjs');
const check = serverRequire('auto-roles-check.cjs');
const { isChatGenerationModel } = serverRequire('chat-model-kind.cjs');
const modelSystem = serverRequire('model-system.cjs');
const quiet = { log() {}, warn() {} };
const plain = createAutoRouter({ roles: () => null });

// ── chat.cjs's lines ────────────────────────────────────────────────────────────────────────
const chatText = fs.readFileSync(path.join(root, 'server/chat.cjs'), 'utf8');
const blocks = {};
function cut(name, start, end) {
  const once = (needle) => {
    const at = chatText.indexOf(needle);
    if (at < 0 || chatText.indexOf(needle, at + 1) >= 0) throw new Error(`chat.cjs holds ${JSON.stringify(needle.slice(0, 60))} ${at < 0 ? 'nowhere' : 'twice'}`);
    return at;
  };
  const from = once(start), to = once(end) + end.length;
  const text = chatText.slice(from, to);
  blocks[name] = { lines: [chatText.slice(0, from).split('\n').length, chatText.slice(0, to).split('\n').length - 1],
    sha256: crypto.createHash('sha256').update(text).digest('hex') };
  return text;
}
const WANTS = cut('wantsAuto', "    const projectProvider = project?.provider === 'lemonade'", '&& autoRoles());\n');
const PROVIDER = cut('provider', '    let provider = routeTarget?.row ?', ': projectProvider || DEFAULT_PROVIDER_ID);\n');
const RESEND = cut('recordResend', '  function recordResend(resend, now) {\n', '    catch { /* logging never breaks a chat */ }\n  }\n');
const ROUTE = cut('route', '    // #1816/#1840: whether a creative turn moves to the Creative role.',
  'pick a chat model in the model popup.` });\n      }\n    }\n');
const AsyncFunction = (async () => {}).constructor;
const PARAMS = ['project', 'autoRoles', 'DEFAULT_PROVIDER_ID', 'getProvider', 'routeTarget', 'chatMachines', 'json', 'res', 'servedCatalogue',
  'missingRoles', 'staleRolesError', 'body', 'modelManager', 'require', 'attachedImages', 'retrievedImages', 'msgs', 'heuristicWantsCode',
  'message', 'codeProject', 'chatSignal', 'engineBusy', 'turnsInFlight', 'classifyFastOrSmart', 'recordOutcome', 'modelsInstalled',
  'lastLoadedModel', 'isChatGenerationModel'];
const runTurn = new AsyncFunction(...PARAMS,
  `${WANTS}${PROVIDER}${RESEND}${ROUTE}return { projectProvider, wantsAuto, model, routedRole, routingDecision };`);

// ── Deterministic inputs ────────────────────────────────────────────────────────────────────
let seed = 0x2026_1113 ^ 0x5b;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const pick = (list) => list[Math.floor(rand() * list.length)];
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));
const thrown = (f) => { try { return { out: f() }; } catch (e) { return { thrown: e?.constructor?.name || 'Error' }; } };

// Messages: greetings, code-ish (fences, each diff start after each line terminator), smart-ish (each keyword, numbers,
// lengths, effort phrases, their word edges), plainly simple asks and their limits, ASCII-only case folding (ſ, the Kelvin
// sign, dotless i), astral and mixed scripts, lone surrogates, non-ASCII digits, and lengths around 160 and 600 units.
const MESSAGES = ['', ' ', 'hi', 'Hello!', 'thanks', 'Good morning, QA team', 'Say hello', 'What is the capital of the QA region?',
  '```js\nqa()\n```', 'see ``` here', '``', 'diff --git a/qa b/qa', 'x\ndiff --git a/qa', 'x\r@@ -1 +1 @@', 'x +++ b/qa', 'x diff --git',
  'x\u0085diff --git', ' diff --git', 'DIFF --GIT', '+++ a/qa', '@@-1', 'Diff --git', 'function', 'a functional QA', 'ALGORITHM', 'Debug this', 'refactoring',
  'implement it', 'optimize', 'architecture', 'regex?', 'sql_table', 'my sql', 'migration.', 'ſql', 'Sql', 'debuG', 'debıg', 'funKtion',
  '1 2', '1 2 3', '1.5, 2,5 and 3', '1.2.3', '1.2.3.4', 'v1 v2 v3', 'a1b2c3', '１２３ ４５６ ７８９', '١ ٢ ٣', 'write 600 words', '10 words', '9 words',
  '100words', '10　words', '10\u0085words', '10 Paragraphs', '12 pages', '20 sentences', '10 wordsmith', 'step by step', 'Step-by-Step',
  'step_by_step', 'in detail', 'in  detail', 'detailed', 'thoroughly', 'comprehensive', 'deep dive', 'prove it', 'derive', 'derived', 'proven',
  'count to 10', 'Count the QA tickets', 'please list three QA tools', 'Please  can you spell QA', 'could you count', 'can you  list', 'tell me a joke',
  'tell me another short joke', 'tell me a short  joke', 'Write me a haiku', 'write a 12-line poem', 'write a 123-line poem', 'write a 500 word story',
  'write a 400-word story', 'write a 401-word story', 'write a 4000 words story', 'write a 0400 word story', 'compose a funny little limerick',
  'make an tiny quick riddle', 'give me another rhyme', 'write a poem about why', 'write a story and explain it', 'write a story in code',
  'list\nthe tools', 'list the tools\n', '  count  ', '﻿count to 3', '\u0085count', 'counting', 'list_tools', 'write a poems', 'write a poem!',
  `count ${'x'.repeat(154)}`, `count ${'x'.repeat(155)}`, `count ${'😀'.repeat(77)}`, `count ${'😀'.repeat(78)}`, 'faſt', 'ſpell it',
  'list 😀', '😀 list', '\ud83dlist', 'list\ud83d', 'Напиши poem 日本語', 'اكتب write a song', '日本語で詩を書いて', 'écris un poème',
  'a'.repeat(600), 'a'.repeat(601), '😀'.repeat(300), `${'😀'.repeat(300)}a`, 'qa '.repeat(250), `${'qa '.repeat(20000)}`];
const WORDS = ['count', 'list', 'spell', 'tell', 'me', 'a', 'another', 'short', 'joke', 'write', 'give', 'make', 'compose', 'poem', 'haiku', 'story',
  'please', 'can', 'you', 'function', 'sql', 'debug', 'step', 'by', 'in', 'detail', 'prove', 'why', 'explain', '12', '300', '1.5', 'words', 'pages',
  'qa', 'diff', '--git', '```', 'code', 'FAST', 'smart'];
const SEPS = [' ', ' ', ' ', '  ', '\t', '\n', '\r', ' ', '\u0085', '﻿', '　', ' ', '', '-', '_', '.', ',', 'é', '😀', '\ud83d', '1'];
for (let n = 0; n < 700; n += 1) {
  let text = rand() < 0.2 ? pick(SEPS) : '';
  for (let i = int(1, 8); i > 0; i -= 1) text += pick(WORDS) + (i > 1 ? pick(SEPS) : '');
  MESSAGES.push(text);
}
const heuristics = MESSAGES.map((message) => ({ message, smart: plain.heuristicWantsSmart(message), code: plain.heuristicWantsCode(message),
  simple: clearlySimple(message) }));

// ── auto-router.cjs ─────────────────────────────────────────────────────────────────────────
const TEXTS = ['FAST', 'smart', 'Code', 'CODE', 'I think SMART. Final: FAST', 'FASTER', 'FAST_', 'FAST1', 'éFAST', 'SMART!', 'faſt', 'ﬆ', 'Ｆａｓｔ',
  'code: fast', 'SMART CODE', '', ' ', 'none', 'fast\ud83d', '\ud83dsmart', 'ß', 'Smartİ', 'codе', 5, 0, true, null, ['fast'], ['smart', 'code'],
  {}, { toString: 'fast' }, [{ toString: 'x' }], undefined];
const verdicts = [];
for (const content of TEXTS) {
  for (const reasoning of [undefined, 'SMART', 'code', 'fast then CODE', { toString: 'x' }]) {
    for (const withCode of [false, true]) {
      const msg = viaJson({ content, reasoning_content: reasoning });
      verdicts.push({ msg, withCode, ...thrown(() => plain.classifierVerdict(msg, withCode)) });
    }
  }
}
for (const msg of [null, 'FAST', 7, [], ['SMART']]) verdicts.push({ msg, withCode: false, ...thrown(() => plain.classifierVerdict(msg || {}, false)) });
const bodies = [];
for (const model of ['qa-fast', undefined, null, 5, { id: 'qa' }]) {
  for (const message of ['Say hello', 'x'.repeat(1001), `${'x'.repeat(999)}😀`, '\ud83d', 'a"b\\c ']) {
    for (const suppress of [true, false]) for (const withCode of [false, true]) bodies.push({ model, message, suppress, withCode, out: plain.classifierBody(model, message, suppress, withCode) });
  }
}
const ROLE_SETS = [null, {}, { fast: 'qa-fast' }, { fast: 'qa-fast', smart: 'qa-smart' }, { fast: 'qa-fast', smart: 'qa-smart', code: 'qa-code' },
  { code: 'qa-code' }, { fast: 'qa-fast', smart: 'qa-smart', code: '' }, { fast: 'qa-fast', smart: 'qa-smart', code: 5 }];
const ok = (body) => ({ ok: true, status: 200, body });
const REPLIES = [[ok({ choices: [{ message: { content: 'SMART' } }] })], [ok({ choices: [{ message: { content: 'CODE' } }] })],
  [ok({ choices: [{ message: { content: '', reasoning_content: 'FAST or SMART? SMART' }, finish_reason: 'length' }] })],
  [ok({ choices: [{ message: { content: '' }, finish_reason: 'length' }] })], [ok({ choices: [] })], [ok({ choices: { 0: { message: { content: 'SMART' } } } })],
  [ok({ choices: 'SMART' })], [ok(null)], [ok({ choices: [null] })], [ok({ choices: [{ message: 'SMART' }] })], [ok({ choices: [{ message: { content: { toString: 'x' } } }] })],
  [{ ok: false, status: 400, body: {} }, ok({ choices: [{ message: { content: 'SMART' } }] })], [{ ok: false, status: 400, body: {} }, { ok: false, status: 400, body: {} }],
  [{ ok: false, status: 400 }, 'throw'], [{ ok: false, status: 500, body: { choices: [{ message: { content: 'SMART' } }] } }],
  [{ ok: true, status: 400, body: { choices: [{ message: { content: 'CODE' } }] } }, ok({ choices: [{ message: { content: 'FAST' } }] })], ['throw']];
const ADMITS = ['none', null, { cause: 'busy-engine' }, { cause: 'Not A Cause' }, 'throw'];
const classify = [];
const addClassify = (roles, message, admit, replies) => {
  const sent = [];
  let n = 0;
  const router = createAutoRouter({ roles: () => roles, provider: () => ({ id: 'default', baseUrl: 'http://qa-engine.invalid:8080/v1' }), headers: () => ({}),
    fetchJson: async (url, init) => { sent.push(init.body); const r = replies[n++]; if (r === 'throw' || r === undefined) throw new Error('qa'); return r; },
    log: quiet, admit: admit === 'none' ? null : async () => { if (admit === 'throw') throw new Error('qa'); return admit; } });
  return router.classify(message).then((out) => classify.push({ roles, message, admit, replies, out, sent }));
};

// ── system-one-router.cjs ───────────────────────────────────────────────────────────────────
const BACKENDS = [null, { id: 'llama-logit', wording: 'compact' }, { id: 'qa-endpoint', wording: 'laya' }, { id: 'llama-logit' }, { wording: 'kev' },
  { id: 'llama-logit', wording: 'STANDARD' }];
const RESULTS = [{ source: 'configured', selected: 'smart', scores: { fast: 0.1, smart: 0.9, qa: 1 }, metadata: { latencyMs: 12.5, model: DECISION_SERVICE_REPORTED_MODEL } },
  { source: 'configured', selected: 'fast', scores: { smart: 0.2, fast: 0.8 }, metadata: { latencyMs: -0, model: 'qa-model-other' } },
  { source: 'configured', selected: 'code', scores: { code: 0.7, fast: '0.1', smart: null }, metadata: { latencyMs: '5' } },
  { source: 'fallback', selected: null, scores: {}, metadata: { fellBack: 'deadline', cause: 'deadline' } },
  { source: 'fallback', selected: 'smart', metadata: { fellBack: 'low-confidence', cause: 'Bad Cause' } },
  { source: 'configured', selected: 'vision', metadata: { fellBack: 'no-backend-answered', cause: 'http-503' } },
  { source: 'configured', selected: 'Smart', metadata: { fellBack: 'other', cause: `a${'b'.repeat(39)}` } },
  { selected: 'fast', scores: [0.5], metadata: 'qa' }, { selected: 'smart', scores: null, metadata: null },
  { source: 'configured', selected: 'fast', metadata: { cause: `a${'b'.repeat(40)}`, fellBack: 'deadline' } }];
const systemOne = [];
const addSystemOne = async (c) => {
  const t = viaJson(c);
  let fallbacks = 0;
  const router = createSystemOneRouter({ enabled: () => t.enabled, roles: () => t.roles, env: {}, getBackend: () => t.backend,
    getDeadlineMs: () => t.deadlineMs, precheck: t.precheck, log: () => {}, fallback: async () => { fallbacks += 1; return t.fallbackRole; } });
  decideCase = { result: t.result };
  const out = await router.classifyWithDetails(t.message);
  systemOne.push({ ...t, out, request: decideCase.request, fallbacks });
  decideCase = null;
};

// ── auto-roles-check.cjs, chat-model-kind.cjs ───────────────────────────────────────────────
const row = (name, extra) => ({ name, loaded: false, ...extra });
const CATALOGUES = [null, undefined, 'qa', {}, [], [row('qa-fast'), row('qa-smart'), row('qa-code'), row('qa-creative'), row('qa-vision')],
  [row('qa-fast', { loaded: true }), row('qa-smart'), row('qa-creative')], [row('qa-fast'), row('qa-smart', { missingFile: true }), row('qa-creative', { loaded: true })],
  [row('qa-one', { loaded: true }), row('qa-creative')], [row('qa-fast'), row('qa-smart'), row('qa-creative', { missingFile: true })],
  [row('qa-fast'), row('qa-smart'), row('qa-embed', { loaded: true, labels: ['Embeddings'] }), row('qa-creative')],
  [row('qa-fast', { loaded: true, system: true }), row('qa-smart', { loaded: true, servedElsewhere: 1 }), row('qa-x', { loaded: 'yes', sidecarProtected: 0 }), row('qa-creative')],
  [row('qa-fast', { loaded: true, labels: 'chat' }), row('qa-smart'), row('qa-creative')], [row('qa-fast', { loaded: true, labels: [{ toString: 'x' }] }), row('qa-creative')],
  [row('qa-fast', { loaded: true, labels: ['chat', 'RERANKER'] }), row('qa-creative')], [row('qa-fast', { loaded: true, labels: ['rerankK'] }), row('qa-creative')],
  [row('qa-fast'), null, row('qa-smart')], [5, 'qa-fast', row('qa-smart', { loaded: true })], [row(5), row('qa-smart'), row({}), row('qa-creative')],
  [row('qa-fast', { loaded: true }), row('qa-creative', { loaded: true })]];
const CHECK_ROLES = [null, {}, { fast: 'qa-fast', smart: 'qa-smart' }, { fast: 'qa-fast', smart: 'qa-smart', creative: 'qa-creative' },
  { fast: 'qa-one', smart: 'qa-one', creative: 'qa-creative', code: 'qa-code', vision: 'qa-vision' }, { fast: 'qa-fast', creative: 'qa-fast' },
  { fast: 5, smart: 'qa-smart', creative: '' }, { fast: { toString: 'x' }, smart: 'qa-smart' }, { fast: ['qa', 'b'], smart: {} }, 'qa'];
const missing = [], creative = [];
for (const r of CHECK_ROLES) {
  for (const cat of CATALOGUES) {
    const roles = viaJson(r), installed = viaJson(cat);
    const m = thrown(() => check.missingRoles(roles, installed));
    missing.push({ roles, installed, ...m, ...(m.out ? { stale: thrown(() => check.staleRolesError(m.out)) } : {}) });
    for (const routed of ['fast', 'smart', 'code', 'creative']) creative.push({ roles, routed, installed, ...thrown(() => check.creativeMoveIsFree(roles, routed, installed)) });
  }
}
const ROLE_FILES = ['', 'not json', '[]', '{"qa-sys":"System-Routing","*":"laya","qa-blank":"  "}', '{"qa-sys":" laya ","laya-qa":"chat","qa-n":5}',
  '{"qa-sys":"system routing","LAYA":"Laya"}'];
const NAMES = ['qa-chat', 'qa-sys', ' qa-sys ', '﻿qa-sys', '\u0085qa-sys', 'laya', 'Laya-qa', 'laya-qa', 'LAYA.x', 'laya_', 'layax', ' laya ', 'lay',
  'qa-embed-model', 'qa-EMBEDDING', 'qa-reRank', 'qa-rerankK', 'qa-embedİ', '', 0, 5, null, true, ['laya'], [], {}, { toString: 'x' }];
const LABELS = [undefined, null, [], ['chat'], ['Embedding'], ['embeddings', 'chat'], ['RERANK'], ['reranking '], ['reranker'], ['rerankK'], 'embedding',
  [5, null, ['embedding']], [{ toString: 'x' }], ['embedding', { toString: 'x' }], [{ toString: 'x' }, 'embedding']];
const chatModels = [];
for (const file of ROLE_FILES) {
  modelSystem.rememberSectionRoles(file);
  const systemRoles = [...modelSystem.sectionRolesOf(file)];
  for (const n of NAMES) for (const l of LABELS) { const name = viaJson(n), labels = viaJson(l); chatModels.push({ systemRoles, name, labels, ...thrown(() => isChatGenerationModel(name, labels)) }); }
}

// ── The turn ────────────────────────────────────────────────────────────────────────────────
const DEFAULT_PROVIDER_ID = 'default';
const PROVIDERS = { default: { id: 'default', label: 'QA engine', baseUrl: 'http://qa-engine.invalid:8080/v1' }, 'qa-cloud': { id: 'qa-cloud', label: 'QA Cloud' },
  'qa-nolabel': { id: 'qa-nolabel', label: '' }, 'qa-odd': { id: 'qa-odd', label: ['QA', 'Odd'] } };
const TURN_PROJECTS = [null, {}, { routing: 'auto' }, { routing: 'manual' }, { routing: 'manual', model: 'qa-chat' }, { routing: 'Auto' },
  { routing: 'auto', provider: 'lemonade' }, { routing: 'auto', provider: 'qa-cloud' }, { routing: 'auto', provider: 'default' },
  { routing: 'manual', provider: 'qa-cloud', model: 'qa-model-cloud' }, { routing: 'manual', provider: 'qa-cloud' }, { routing: 'manual', provider: 'qa-nolabel' },
  { routing: 'manual', provider: 'qa-odd' }, { routing: 'manual', provider: 'qa-missing' }, { routing: 'manual', provider: 5 }, { model: 'qa-embed-model' },
  { model: 'laya-qa' }, { model: 'qa-sys' }, { model: 'qa-labelled' }, { model: 'qa-fast' }, { provider: 'lemonade', model: 'qa-chat' },
  { routing: 'auto', contextProfile: 'high' }, { routing: 'auto', contextProfile: 'High' }, { model: 5 }, { model: '' }, { model: {} }, { model: [] },
  { model: { toString: 'x' } }, { routing: 'auto', model: 'qa-chat' }];
const TURN_ROLES = [null, {}, { fast: 'qa-fast', smart: 'qa-smart' }, { fast: 'qa-one', smart: 'qa-one' }, { fast: 'qa-one', smart: 'qa-one', code: 'qa-code' },
  { fast: 'qa-one', smart: 'qa-one', code: 'qa-one', creative: 'qa-one' }, { fast: 'qa-fast', smart: 'qa-smart', code: 'qa-code', creative: 'qa-creative' },
  { fast: 'qa-fast', creative: 'qa-fast' }, { smart: 'qa-smart' }, { fast: 'qa-fast' }, { fast: 'qa-fast', smart: 'qa-smart', vision: 'qa-vision' },
  { fast: 'qa-one', smart: 'qa-one', creative: 'qa-creative' }, { fast: 5, smart: '' }, { fast: { toString: 'x' }, smart: 'qa-smart' }];
const TURN_CATALOGUES = [null, [], [row('qa-fast', { loaded: true }), row('qa-smart'), row('qa-code'), row('qa-creative'), row('qa-vision'), row('qa-one'), row('qa-chat')],
  [row('qa-fast'), row('qa-smart'), row('qa-code'), row('qa-creative', { loaded: true }), row('qa-one'), row('qa-vision')],
  [row('qa-fast'), row('qa-smart'), row('qa-code'), row('qa-one'), row('qa-chat')], [row('qa-fast'), row('qa-one'), row('qa-creative')],
  [row('qa-chat'), row('qa-labelled', { labels: ['embedding'] }), row('qa-embed-model'), row('qa-sys')], [row('qa-fast'), null, row('qa-smart')],
  [row('qa-fast'), row('qa-smart'), row('qa-creative', { missingFile: true }), row('qa-one'), row('qa-code')]];
const routing = (role, status) => ({ offered: [], scores: {}, selectedRole: role, effectiveRole: role, backend: 'legacy', model: null, calibrated: false,
  latencyMs: null, status, fallbackReason: status === 'fallback' ? 'disabled' : null, fallbackCause: null });
const CLASSIFICATIONS = ['fast', 'smart', 'code', { role: 'fast', routingDecision: routing('fast', 'accepted') }, { role: 'smart', routingDecision: routing(null, 'fallback') },
  { role: 'code', routingDecision: null }, { role: 'smart', routingDecision: { status: '', effectiveRole: 'x', zeta: 1 } }, { role: 'fast', routingDecision: { 0: 'qa', status: 'accepted' } }];
const TURN_MESSAGES = ['Say hello', 'write a poem about QA', 'Write a short story', '```\ncode\n```', 'diff --git a/x b/x', 'compose a song'];
const RESENDS = [undefined, { kind: 'regenerate', role: 'smart', status: 'accepted' }, { kind: 'retry', role: 'vision', status: 'x' }, { kind: 'other' }, 'retry'];
const turns = [];
async function addTurn(raw) {
  const t = viaJson(raw);
  const calls = [];
  modelSystem.rememberSectionRoles(t.rolesFile || '');
  let refusal = null, reads = 0;
  const ends = [];
  const env = {
    // `savedRoles`, when given, is what every later autoRoles() reads (a workspace changed between the two reads).
    project: t.project, autoRoles: () => { calls.push('autoRoles'); reads += 1; return reads > 1 && 'savedRoles' in t ? t.savedRoles : t.autoRoles; },
    DEFAULT_PROVIDER_ID, getProvider: (id) => { calls.push(`getProvider:${JSON.stringify(id)}`); return PROVIDERS[id] || PROVIDERS.default; },
    routeTarget: null, chatMachines: null,
    json: (res, status, payload) => { refusal = { status, body: payload }; return 'refused'; }, res: { once: (event, f) => ends.push(f) },
    servedCatalogue: async () => { calls.push('servedCatalogue'); return t.catalogue; },
    missingRoles: check.missingRoles, staleRolesError: check.staleRolesError, body: { message: t.message, compactOnly: t.compactOnly, mode: t.mode, resend: t.resend },
    modelManager: { enabled: t.managerEnabled }, require: (f) => (f === './route-preload.cjs'
      ? { ...serverRequire('route-preload.cjs'), startPreload: async ({ model }) => {
        calls.push(`startPreload:${JSON.stringify(model)}`);
        // A fake pre-load (the engine's own checks are startPreload's; `preloadStarts: false` is one that declined).
        return t.preloadStarts === false ? null : { model, use: () => calls.push('preload.use'), cancel: () => calls.push('preload.cancel'),
          releaseIfUnused: async () => { calls.push('preload.releaseIfUnused'); } };
      } }
      : serverRequire(f.replace(/^\.\//, ''))),
    attachedImages: t.images ? [{}] : [], retrievedImages: [], msgs: [{ role: 'user', content: t.message }], heuristicWantsCode: plain.heuristicWantsCode,
    message: t.message, codeProject: () => { calls.push('codeProject'); return !!t.codeProject; }, chatSignal: { signal: { aborted: !!t.aborted } },
    engineBusy: () => false, turnsInFlight: 1, classifyFastOrSmart: async (m) => { calls.push('classify'); return t.classification; },
    recordOutcome: (entry) => { calls.push(`recordOutcome:${JSON.stringify(entry)}`); }, modelsInstalled: async () => { calls.push('modelsInstalled'); if (t.installedThrows) throw new Error('qa'); },
    lastLoadedModel: () => { calls.push('lastLoadedModel'); return t.lastLoaded; }, isChatGenerationModel,
  };
  let out;
  try {
    const r = await runTurn(...PARAMS.map((p) => env[p]));
    out = r === 'refused' ? { refusal } : r;
  } catch (e) { out = { thrown: e?.constructor?.name || 'Error' }; }
  // The response ends: its finish and close handlers run.
  for (const f of ends) f();
  const provider = calls.find((c) => c.startsWith('getProvider:'));
  turns.push({ ...t, systemRoles: [...modelSystem.sectionRolesOf(t.rolesFile || '')],
    provider: provider && (PROVIDERS[JSON.parse(provider.slice('getProvider:'.length))] || PROVIDERS.default), out, calls });
}
const turn = (extra) => ({ project: null, autoRoles: { fast: 'qa-fast', smart: 'qa-smart' }, catalogue: TURN_CATALOGUES[2], classification: 'fast',
  message: 'Say hello', managerEnabled: true, lastLoaded: 'qa-loaded', ...extra });

(async () => {
  for (const roles of ROLE_SETS) for (const message of MESSAGES.slice(0, 40)) await addClassify(viaJson(roles), message, null, viaJson(REPLIES[0]));
  for (const roles of ROLE_SETS.slice(3, 5)) for (const admit of ADMITS) for (const replies of REPLIES) await addClassify(viaJson(roles), 'Say hello', viaJson(admit), viaJson(replies));
  for (const enabled of [true, false]) {
    for (const backend of BACKENDS) {
      for (const roles of ROLE_SETS) await addSystemOne({ enabled, backend, roles, message: 'Explain QA', result: RESULTS[0], deadlineMs: 1500, fallbackRole: 'smart' });
    }
  }
  for (const result of RESULTS) {
    for (const roles of ROLE_SETS.slice(3, 5)) {
      for (const backend of BACKENDS.slice(1)) for (const deadlineMs of [1500, undefined, '2000']) await addSystemOne({ enabled: true, backend, roles, message: 'Explain QA', result, deadlineMs, fallbackRole: 'fast' });
    }
  }
  for (const message of MESSAGES.slice(0, 160)) await addSystemOne({ enabled: true, backend: BACKENDS[1], roles: ROLE_SETS[4], message, result: RESULTS[1], deadlineMs: 900, fallbackRole: 'code' });
  await addSystemOne({ enabled: true, backend: BACKENDS[1], roles: ROLE_SETS[4], message: 'count to 3', result: RESULTS[0], deadlineMs: 900, fallbackRole: 'code', precheck: false });
  // The turn: each axis on its own, then a generated corpus over everything at once.
  for (const project of TURN_PROJECTS) for (const autoRoles of [null, TURN_ROLES[2]]) for (const catalogue of TURN_CATALOGUES.slice(0, 3)) await addTurn(turn({ project, autoRoles, catalogue }));
  for (const autoRoles of TURN_ROLES) {
    for (const catalogue of TURN_CATALOGUES) for (const message of TURN_MESSAGES) await addTurn(turn({ autoRoles, catalogue, message, classification: pick(CLASSIFICATIONS) }));
  }
  for (const classification of CLASSIFICATIONS) for (const autoRoles of TURN_ROLES) for (const resend of RESENDS) await addTurn(turn({ autoRoles, classification, resend }));
  for (const project of [null, { routing: 'manual' }, { routing: 'manual', model: 'qa-chat' }]) {
    for (const managerEnabled of [true, false, 0]) {
      for (const lastLoaded of [null, 'qa-loaded', 'qa-embed-x', 'laya']) {
        for (const installedThrows of [false, true]) await addTurn(turn({ project, autoRoles: null, managerEnabled, lastLoaded, installedThrows }));
      }
    }
  }
  for (const project of [{ model: 'qa-sys' }, { model: 'laya-qa' }, { model: ' qa-sys ' }]) {
    for (const rolesFile of ROLE_FILES) await addTurn(turn({ project, autoRoles: null, rolesFile, catalogue: TURN_CATALOGUES[6] }));
  }
  for (const autoRoles of TURN_ROLES.slice(2, 8)) {
    for (const extra of [{ compactOnly: true }, { mode: 'cowork' }, { images: true }, { codeProject: true }, { managerEnabled: false }, { project: { routing: 'auto', contextProfile: 'high' } }]) {
      for (const message of ['Say hello', '```x```']) await addTurn(turn({ autoRoles, catalogue: TURN_CATALOGUES[3], message, ...extra }));
    }
  }
  // The pre-load and its cleanup (#1863): kept, abandoned (with and without an abort), declined, never asked; codeProject
  // read only when it decides; the second autoRoles() read finding no roles (the 400).
  const ONE_CODE = { fast: 'qa-one', smart: 'qa-one', code: 'qa-code' };
  for (const classification of ['fast', 'smart', 'code', { role: 'code', routingDecision: routing('code', 'accepted') }]) {
    for (const extra of [{}, { aborted: true }, { preloadStarts: false }, { codeProject: true }, { mode: 'cowork' }, { message: '```x```' },
      { resend: RESENDS[1] }, { catalogue: [row('qa-one'), row('qa-code'), null] }, { savedRoles: null }, { savedRoles: {} }]) {
      for (const autoRoles of [ONE_CODE, TURN_ROLES[3], TURN_ROLES[5], { fast: 'qa-one', smart: 'qa-one', code: 'qa-code', creative: 'qa-one' }]) {
        await addTurn(turn({ autoRoles, catalogue: TURN_CATALOGUES[3], classification, ...extra }));
      }
    }
  }
  for (const savedRoles of [null, {}, 0, '', TURN_ROLES[2], { fast: 'qa-gone', smart: 'qa-smart' }]) {
    for (const project of [null, { routing: 'auto' }, { routing: 'manual' }]) await addTurn(turn({ project, savedRoles }));
  }
  for (let n = 0; n < 1500; n += 1) {
    await addTurn({ project: pick(TURN_PROJECTS), autoRoles: pick(TURN_ROLES), catalogue: pick(TURN_CATALOGUES), classification: pick(CLASSIFICATIONS),
      message: pick(TURN_MESSAGES), managerEnabled: rand() < 0.8, lastLoaded: pick([null, 'qa-loaded', 'qa-embed-x']), compactOnly: rand() < 0.1,
      mode: rand() < 0.1 ? 'cowork' : undefined, images: rand() < 0.1, codeProject: rand() < 0.2, resend: pick(RESENDS), rolesFile: pick(ROLE_FILES),
      aborted: rand() < 0.1, preloadStarts: rand() < 0.7, ...(rand() < 0.05 ? { savedRoles: pick([null, TURN_ROLES[2]]) } : {}) });
  }

  // ── Output ────────────────────────────────────────────────────────────────────────────────
  const sources = sourcesRead();
  const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
  const text = `{"version":1,"generator":"crates/chat-prepare/tests/record_roles.cjs",\n"node":${JSON.stringify(process.version)},\n`
    + `"sources":${JSON.stringify(sources)},\n"blocks":${JSON.stringify(blocks)},\n"defaultProviderId":${JSON.stringify(DEFAULT_PROVIDER_ID)},\n`
    + `"serviceModel":${JSON.stringify(DECISION_SERVICE_REPORTED_MODEL)},\n`
    + `"heuristics":[\n${lines(heuristics)}\n],\n"verdicts":[\n${lines(verdicts)}\n],\n"bodies":[\n${lines(bodies)}\n],\n`
    + `"classify":[\n${lines(classify)}\n],\n"systemOne":[\n${lines(systemOne)}\n],\n"missing":[\n${lines(missing)}\n],\n`
    + `"creative":[\n${lines(creative)}\n],\n"chatModels":[\n${lines(chatModels)}\n],\n"turns":[\n${lines(turns)}\n],\n"differences":[]}\n`;
  const at = process.argv.indexOf('--check');
  const refused = turns.filter((c) => c.out.refusal).length, auto = turns.filter((c) => c.out.wantsAuto).length;
  const counts = `${heuristics.length} heuristics, ${verdicts.length} verdicts, ${bodies.length} bodies, ${classify.length} classify, `
    + `${systemOne.length} systemOne, ${missing.length} missing, ${creative.length} creative, ${chatModels.length} chatModels, `
    + `${turns.length} turns (${auto} Automatic, ${refused} refused)`;
  if (at > 0) {
    const committed = fs.readFileSync(process.argv[at + 1], 'utf8');
    if (committed !== text) {
    // A host that cannot run Node takes the fresh recording from CI (chat-prepare.yml uploads RECORD_FRESH).
    if (process.env.RECORD_FRESH) fs.writeFileSync(process.env.RECORD_FRESH, text);
    process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
    process.stderr.write(`ok: ${counts} recorded as committed\n`);
  } else {
    process.stderr.write(`recorded: ${counts}\n`);
    process.stdout.write(text);
  }
})().catch((e) => { process.stderr.write(`${e?.stack || e}\n`); process.exit(1); });
