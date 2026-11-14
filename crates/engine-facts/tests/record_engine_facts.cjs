#!/usr/bin/env node
'use strict';
// Recorded facts for crates/engine-facts (track R11): what core answers on POST /internal/code/engine for one Code
// start, and the provider pieces it is built from. The contract src/provider.rs, src/code.rs and src/shared.rs are
// replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout with server/wasm/dav-parse.wasm> \
//     node crates/engine-facts/tests/record_engine_facts.cjs > crates/engine-facts/tests/fixtures/node-engine-facts.v1.json
//   ... node crates/engine-facts/tests/record_engine_facts.cjs --check crates/engine-facts/tests/fixtures/node-engine-facts.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// The checkout needs server/wasm/dav-parse.wasm (the dav-parse.lock pin: isExternalProvider and frameUntrusted are the
// Rust ports in it); nothing from node_modules. Every expectation comes from noevia-core's own code; the inputs are
// synthetic and QA-named (hand-made edges plus a seeded PRNG), passed through JSON first so Node reads exactly what is
// recorded (a field left out of a case is `undefined`):
//   engineCaps  providers.cjs engineCapabilities(kind)
//   effective   providers.cjs effectiveCapabilities(row) (with parseCapabilities behind it)
//   lookup      providers.cjs createProviderRegistry({ PROVIDERS: view, DEFAULT_PROVIDER_ID }).getProvider(id)
//   headers     the same registry's providerHeaders(provider, extra)
//   external    provider-egress.cjs isExternalProvider(row) (the real port in dav-parse.wasm)
//   target      code-machine.cjs codeTargetFrom(chatMachines, autoRoles) over a stub chatMachines.target(role) and
//               autoRoles(); `calls` lists what was asked, in order
//   forCode     shared-context.cjs forCode(project, loadChats(project.id)) with projects.cjs's own loadChats and
//               sanitizeChats (cut out of the pinned projects.cjs)
//   bridge      rust-code-executor.cjs createCodeEngineBridge wired exactly as index.cjs wires it: index.cjs's own lines
//               (the default provider constants and row, codeMachineTarget, codeRoleEndpoint, codeEngine,
//               codeSharedContext and the bridge's construction, cut out of the pinned index.cjs and run as they are)
//               over the real providers.cjs registry, code-machine.cjs, shared-context.cjs and provider-egress.cjs, with
//               stubs for the environment, chatMachines.target, autoRoles, lastLoadedModel, tuningActive, the feature
//               flags and getProject. Recorded: the status and body of one trusted POST.
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
const server = path.join(root, 'server');
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => process.stderr.write(`${a.map(String).join(' ')}\n`);
const SOURCE_FILES = ['server/index.cjs', 'server/projects.cjs', 'server/providers.cjs', 'server/provider-egress.cjs',
  'server/code-machine.cjs', 'server/shared-context.cjs', 'server/prompt-framing.cjs', 'server/rust-code-executor.cjs',
  'server/trusted-front.cjs', 'server/wasm/dav-parse.wasm'];
const sources = Object.fromEntries(SOURCE_FILES.map((f) => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex')]));
const req = (f) => require(path.join(server, f));
const providers = req('providers.cjs');
const egress = req('provider-egress.cjs');
const codeMachine = req('code-machine.cjs');
const shared = req('shared-context.cjs');
const codeExecutor = req('rust-code-executor.cjs');

// ── The lines of index.cjs and projects.cjs that are run as they are ────────────────────────
const blocks = {};
function cutter(file) {
  const text = fs.readFileSync(path.join(server, file), 'utf8');
  return (name, start, end) => {
    const from = text.indexOf(start);
    if (from < 0 || text.indexOf(start, from + 1) >= 0) throw new Error(`${file} holds ${JSON.stringify(start.slice(0, 60))} ${from < 0 ? 'nowhere' : 'twice'}`);
    const stop = text.indexOf(end, from);
    if (stop < 0) throw new Error(`${file}: no ${JSON.stringify(end.slice(0, 60))} after ${name}`);
    const to = stop + end.length;
    const cut = text.slice(from, to);
    blocks[name] = { file: `server/${file}`, lines: [text.slice(0, from).split('\n').length, text.slice(0, to).split('\n').length - (cut.endsWith('\n') ? 1 : 0)],
      sha256: crypto.createHash('sha256').update(cut).digest('hex') };
    return cut;
  };
}
const fromIndex = cutter('index.cjs');
const CONSTS = fromIndex('constants', 'const INFERENCE_BASE = ', "|| (process.env.LEMONADE_BASE_URL ? 'lemonade' : 'none');\n");
const CAPS = fromIndex('capabilities', 'const DEFAULT_PROVIDER_CAPABILITIES = ', ';\n');
const ROW = fromIndex('defaultRow', '{ id: DEFAULT_PROVIDER_ID, label: DEFAULT_PROVIDER_LABEL', '{ capabilities: DEFAULT_PROVIDER_CAPABILITIES } : {}) }');
const TARGET = fromIndex('codeMachineTarget', 'const codeMachineTarget = () =>', ';\n');
const ROLE = fromIndex('codeRoleEndpoint', 'const codeRoleEndpoint = () => {', '\n};\n');
const ENGINE = fromIndex('codeEngine', 'const codeEngine = () => {', '\n};\n');
const CONTEXT = fromIndex('codeSharedContext', 'const codeSharedContext = ', ';\n');
const BRIDGE = fromIndex('bridge', 'const codeEngineBridge = codeExecutor.createCodeEngineBridge({', '\n});\n');
const fromProjects = cutter('projects.cjs');
const SANITIZE = fromProjects('sanitizeChats', '  function sanitizeChats(chats) {', '\n  }\n');
const LOAD = fromProjects('loadChats', '  function loadChats(projectId) {', '\n  }\n');

const SECRET = 'qa-front-secret';
const PARAMS = ['process', 'require', 'codeExecutor', 'chatMachines', 'autoRoles', 'lastLoadedModel', 'modelManager', 'features',
  'getProject', 'currentWorkspace', 'codeEngineRelay', 'createProviderRegistry', 'extraRows', 'FRONT_SECRET', 'json', 'readJson',
  'diaryAsUser'];
const buildBridge = new Function(...PARAMS, `${CONSTS}${CAPS}
const defaultProvider = (${ROW});
const PROVIDERS = [{ ...defaultProvider, shared: true }, ...extraRows];
const { getProvider } = createProviderRegistry({ PROVIDERS, DEFAULT_PROVIDER_ID });
${SANITIZE}${LOAD}${TARGET}${ROLE}${ENGINE}${CONTEXT}${BRIDGE}return codeEngineBridge;`);
const localRequire = (id) => (id.startsWith('./') ? require(path.join(server, id)) : require(id));

// ── Deterministic inputs ────────────────────────────────────────────────────────────────────
let seed = 0x2026_1114;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const pick = (list) => list[Math.floor(rand() * list.length)];
const maybe = (p, v) => (rand() < p ? v : undefined);
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));
const clean = (o) => Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined));

const KINDS = [undefined, '', 'none', 'llamacpp', 'LLAMACPP', 'llamacpp ', 'lemonade', 'constructor', 'toString', '__proto__',
  'hasOwnProperty', 'qa-kind'];
const CAPABILITIES = [undefined, null, {}, [], 'qa', 5, true, { jsonSchemaParam: true }, { jsonSchemaParam: 'yes' },
  { reasoningEffortParam: true, reasoningEffortModels: ['qa-model', ' qa-trim '] }, { reasoningEffortModels: [] },
  { reasoningEffortModels: ['', 'qa'] }, { reasoningEffortModels: [5] }, { reasoningEffortModels: 'qa' },
  { reasoningEffortModels: Array.from({ length: 51 }, (_, i) => `qa-${i}`) }, { reasoningEffortModels: ['x'.repeat(200)] },
  { reasoningEffortModels: ['x'.repeat(201)] }, { tokenBudgetField: 'max_tokens' }, { tokenBudgetField: 'max_completion_tokens' },
  { tokenBudgetField: 'qa' }, { unknown: true }, { jsonSchemaParam: false, tokenBudgetField: 'max_tokens', reasoningEffortParam: false },
  { tokenBudgetField: 'max_tokens', jsonSchemaParam: true, reasoningEffortModels: ['b', 'a'], reasoningEffortParam: true }];
const BASES = [undefined, null, '', 5, 'http://qa-engine.invalid:8080', 'http://qa-engine.invalid:8080/', 'http://qa-engine.invalid/v1',
  'http://qa-engine.invalid/v1/', 'http://qa-engine.invalid/v1//', 'http://qa-engine.invalid//', 'https://api.openai.com',
  'https://api.openai.com/', 'https://api.openai.com/v1', 'https://api.openai.com/v1/', 'https://API.OpenAI.com/v1',
  'https://api.openai.com/v2', 'https://api.openai.com/v1?x=1', 'https://api.openai.com/v1#f', 'https://user:pw@api.openai.com/v1',
  'https://api.openai.com:443/v1', 'https://api.openai.com:8443/v1', 'http://api.openai.com/v1', 'not a url', 'https://qa.example.invalid/v1',
  'https://openrouter.ai/api/v1', 'https://api.groq.com/openai/v1', 'https://qa.openrouter.ai/v1', 'https://user:pw@qa.openrouter.ai/v1',
  'https://xn--qa-ufa.invalid/v1', 'https://qa%41.invalid/', 'https://qä.invalid/v1', 'http://[::1]:8080/v1', 'http://127.0.0.1:11434'];
const KEYS = [undefined, null, '', 'local', 'qa-key', 'qa key with spaces', 5, 0, true];
const IDS = [undefined, null, 'default', 'lemonade', 'qa-provider', 'qa-other', 'chatgpt', '', 5, 'qa-local'];

// ── engineCapabilities, effectiveCapabilities ───────────────────────────────────────────────
const engineCaps = KINDS.map((kind) => ({ ...clean({ kind }), out: viaJson(providers.engineCapabilities(kind)) ?? null }));
const effective = [];
for (const capabilities of CAPABILITIES) for (const baseUrl of [undefined, 'https://api.openai.com/v1']) {
  const row = viaJson(clean({ id: 'qa-provider', baseUrl, capabilities }));
  effective.push({ row, out: providers.effectiveCapabilities(row) });
}
for (const baseUrl of BASES) {
  const row = viaJson(clean({ id: 'qa-provider', baseUrl }));
  effective.push({ row, out: providers.effectiveCapabilities(row) });
}

// ── getProvider, providerHeaders ────────────────────────────────────────────────────────────
const randomRow = () => viaJson(clean({ id: pick(IDS), label: maybe(0.8, pick(['QA engine', 'QA cloud', ''])), baseUrl: pick(BASES),
  apiKey: pick(KEYS), shared: maybe(0.3, pick([true, false])), kind: maybe(0.2, pick(['chatgpt-oauth', 'qa'])),
  external: maybe(0.2, pick([true, false, 'true'])), capabilities: maybe(0.4, pick(CAPABILITIES)), keyUnreadable: maybe(0.1, true) }));
const lookup = [];
for (let n = 0; n < 400; n += 1) {
  const view = Array.from({ length: Math.floor(rand() * 5) }, randomRow);
  const id = pick(IDS);
  const defaultId = pick(['default', 'qa-local', 'lemonade']);
  const { getProvider } = providers.createProviderRegistry({ PROVIDERS: view, DEFAULT_PROVIDER_ID: defaultId });
  lookup.push({ view, ...clean({ id }), defaultId, out: viaJson(getProvider(id)) ?? null });
}
const headers = [];
const { providerHeaders } = providers.createProviderRegistry({ PROVIDERS: [], DEFAULT_PROVIDER_ID: 'default' });
for (const apiKey of KEYS) for (const extra of [undefined, null, {}, { Accept: 'text/event-stream' }, { 'Content-Type': 'text/plain' },
  { Authorization: 'Bearer qa-extra' }]) {
  const provider = viaJson(clean({ id: 'qa-provider', apiKey }));
  headers.push({ provider, ...clean({ extra }), out: providerHeaders(provider, extra) });
}

// ── isExternalProvider ──────────────────────────────────────────────────────────────────────
const external = [{ row: null, out: egress.isExternalProvider(null) }];
for (const baseUrl of BASES) for (const variant of [{}, { kind: 'chatgpt-oauth' }, { kind: 'qa' }, { external: true }, { external: 'true' }]) {
  const row = viaJson(clean({ id: 'qa-provider', label: 'QA', baseUrl, ...variant }));
  external.push({ row, out: egress.isExternalProvider(row) });
}

// ── codeTargetFrom ──────────────────────────────────────────────────────────────────────────
const TARGETS = [null, 'throw', { machine: { id: 'qa-machine' }, baseUrl: 'http://qa-machine.invalid:8080', model: 'qa-code', port: 8080 },
  { machine: { id: 'qa-machine-2' }, baseUrl: 'http://qa-machine-2.invalid:9000', model: 'qa-smart', port: 9000 }];
const ROLES = [null, 'throw', {}, { code: 'qa-code' }, { code: '' }, { smart: 'qa-smart' }, { code: 0 }, { code: 'qa-code', smart: 'qa-smart' },
  'qa', 5, [], ['qa-code']];
function stubMachines(code, smart, calls) {
  return { target(role) {
    calls.push(`target:${role}`);
    const t = role === 'code' ? code : role === 'smart' ? smart : null;
    if (t === 'throw') throw Object.assign(new Error('machines unavailable'), { code: 'machinesUnavailable' });
    return viaJson(t);
  } };
}
const target = [];
for (const code of TARGETS) for (const smart of TARGETS) for (const roles of ROLES) {
  const calls = [];
  const autoRoles = () => { calls.push('autoRoles'); if (roles === 'throw') throw new Error('roles unreadable'); return viaJson(roles); };
  let out;
  try { out = { out: codeMachine.codeTargetFrom(stubMachines(code, smart, calls), autoRoles) }; } catch (e) { out = { throws: e.code || e.constructor.name }; }
  target.push({ code, smart, roles, ...out, calls });
}

// ── forCode over loadChats ──────────────────────────────────────────────────────────────────
const WORDS = ['QA', 'alpha', 'notes', 'Ünïcode', '日本語', '😀', '𝒳qa', 'tab\there', 'line\nbreak', 'cr\r\nlf', 'quote"qa', '  ', ' ',
  '　', '\ud83d', '\ude00', '</untrusted>', '<<END>>', '$&', ' ', ' ', '﻿', '\u0085'];
const words = (n) => Array.from({ length: n }, () => pick(WORDS)).join(pick([' ', '', '\n']));
const long = (n) => words(n) + 'x'.repeat(Math.floor(rand() * 4)) + '😀'.repeat(Math.floor(rand() * 3));
function randomChat(i) {
  return clean({ id: maybe(0.9, pick([`qa-chat-${i}`, 5])), title: maybe(0.9, pick([long(3), long(40), '', 0, 7, true, { a: 1 }, ['qa', 'b']])),
    preview: maybe(0.6, pick([long(5), long(60), '', 0, null])), updatedAt: maybe(0.9, pick([1000 + i, 5000 - i, 0, null, true, 1000 + i])) });
}
function randomProject(i) {
  const chats = rand() < 0.1 ? pick([null, 'qa', 5, {}, true]) : Array.from({ length: Math.floor(rand() * 12) }, (_, k) => (rand() < 0.05 ? pick([null, 5, 'qa']) : randomChat(k)));
  return clean({ id: `qa-project-${i}`, name: maybe(0.95, pick([long(2), long(30), '', 5])),
    sharedContext: pick([undefined, null, {}, { code: true }, { code: true }, { code: true, chat: true }, { code: 'true' }, { chat: true }, [], 'code']),
    goal: maybe(0.6, pick([long(4), long(150), '', 0, 12, { qa: 1 }])), instructions: maybe(0.6, pick([long(10), long(700), '', 5, ['a', 'b'], { qa: 1 }])),
    memories: maybe(0.6, pick([[], [long(2), '', '  ', 5, null, long(90)], Array.from({ length: 25 }, (_, k) => `QA memory ${k}`), 'qa', {}, 5])),
    chats });
}
const forCodeCases = [];
const loadChatsFor = (project) => new Function('getProject', `${SANITIZE}${LOAD}return loadChats;`)(() => project);
function attempt(f) { try { return { out: f() }; } catch (e) { return { throws: e.constructor.name }; } }
for (let n = 0; n < 500; n += 1) {
  const project = viaJson(randomProject(n));
  forCodeCases.push({ project, ...attempt(() => shared.forCode(project, loadChatsFor(project)(project.id))) });
}

// ── The bridge ──────────────────────────────────────────────────────────────────────────────
const ENVS = [{}, { INFERENCE_BASE_URL: 'http://qa-engine.invalid:8080', MODEL_MANAGER_KIND: 'llamacpp' },
  { INFERENCE_BASE_URL: 'http://qa-engine.invalid:8080/v1/', INFERENCE_API_KEY: 'qa-inference-key', MODEL_MANAGER_KIND: 'llamacpp' },
  { LEMONADE_BASE_URL: 'http://qa-lemonade.invalid:8000', LEMONADE_API_KEY: 'qa-lemonade-key' },
  { INFERENCE_BASE_URL: 'https://openrouter.ai/api/v1', INFERENCE_API_KEY: 'qa-cloud-key', DEFAULT_PROVIDER_ID: 'qa-local' },
  { INFERENCE_BASE_URL: 'https://api.openai.com/v1', MODEL_MANAGER_KIND: 'none', DEFAULT_PROVIDER_LABEL: 'QA label' },
  { INFERENCE_BASE_URL: '', LEMONADE_BASE_URL: '', MODEL_MANAGER_KIND: '' }];
const CODE_ENVS = [{}, { CODE_ENGINE_URL: 'http://qa-code-engine.invalid:9000' }, { CODE_ENGINE_URL: 'http://qa-code-engine.invalid:9000/v1///' },
  { CODE_ENGINE_URL: '', CODE_ENGINE_API_KEY: '' }, { CODE_ENGINE_API_KEY: 'qa-code-key' }, { CODE_CONTEXT_TOKENS: '32768' },
  { CODE_CONTEXT_TOKENS: ' 0x10 ' }, { CODE_CONTEXT_TOKENS: '1e3' }, { CODE_CONTEXT_TOKENS: 'qa' }, { CODE_CONTEXT_TOKENS: '0' },
  { CODE_CONTEXT_TOKENS: '-5' }, { CODE_CONTEXT_TOKENS: '2.5' }, { CODE_CONTEXT_TOKENS: 'Infinity' }, { CODE_CONTEXT_TOKENS: '' },
  { CODE_ENGINE_URL: 'http://qa-code-engine.invalid:9000', CODE_ENGINE_API_KEY: 'qa-code-key', CODE_CONTEXT_TOKENS: '65536' }];
const BRIDGE_ROLES = [null, {}, { code: 'qa-code' }, { smart: 'qa-smart' }, { code: 'qa-code', smart: 'qa-smart' }, { code: '', smart: 0 }, 'qa', 5, [],
  { code: 7 }, { smart: { qa: 1 } }];
const LAST = [null, 'qa-loaded', '', 0];
const PAUSED = [false, true, 'true', 1, undefined];
const bridge = [];
async function addBridge(c) {
  const calls = [];
  const env = viaJson({ ...c.env, ...c.codeEnv });
  const handle = buildBridge({ env }, localRequire, codeExecutor, stubMachines(c.code, c.smart, calls),
    () => viaJson(c.roles), () => viaJson(c.lastLoaded), { tuningActive: () => { calls.push('tuningActive'); return c.paused; } },
    { enabled: (name) => { calls.push(`feature:${name}`); return c.features.includes(name); } },
    (id) => { calls.push('getProject'); return id === c.project?.id ? viaJson(c.project) : null; }, () => ({}), { url: 'http://qa-relay.invalid:8095/v1', relay: {} },
    providers.createProviderRegistry, viaJson(c.extraRows), SECRET,
    (res, status, body) => { res.status = status; res.body = viaJson(body); }, async () => viaJson(c.body), (user, fn) => fn());
  const reqStub = { url: '/internal/code/engine', method: 'POST', headers: { 'x-noevia-store-token': SECRET }, socket: { remoteAddress: '127.0.0.1' } };
  const res = {};
  const handled = await handle(reqStub, res);
  bridge.push({ ...c, handled, status: res.status, answer: res.body, calls });
}

(async () => {
  for (let n = 0; n < 1200; n += 1) {
    const project = rand() < 0.08 ? null : viaJson(randomProject(n));
    await addBridge({ env: pick(ENVS), codeEnv: pick(CODE_ENVS), roles: pick(BRIDGE_ROLES), lastLoaded: pick(LAST),
      code: rand() < 0.75 ? null : pick(TARGETS), smart: rand() < 0.75 ? null : pick(TARGETS), paused: pick(PAUSED),
      features: pick([[], ['codePipeline'], ['plannerReview'], ['codePipeline', 'plannerReview']]),
      extraRows: rand() < 0.3 ? [randomRow(), randomRow()] : [], project,
      body: { user: 'qa-user', projectId: project ? project.id : 'qa-missing' } });
  }

  // ── Output ────────────────────────────────────────────────────────────────────────────────
  const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
  const text = `{"version":1,"generator":"crates/engine-facts/tests/record_engine_facts.cjs",\n"node":${JSON.stringify(process.version)},\n`
    + `"sources":${JSON.stringify(sources)},\n"blocks":${JSON.stringify(blocks)},\n`
    + `"engineCaps":[\n${lines(engineCaps)}\n],\n"effective":[\n${lines(effective)}\n],\n"lookup":[\n${lines(lookup)}\n],\n`
    + `"headers":[\n${lines(headers)}\n],\n"external":[\n${lines(external)}\n],\n"target":[\n${lines(target)}\n],\n`
    + `"forCode":[\n${lines(forCodeCases)}\n],\n"bridge":[\n${lines(bridge)}\n],\n"differences":[]}\n`;
  const counts = `${engineCaps.length} engineCaps, ${effective.length} effective, ${lookup.length} lookup, ${headers.length} headers, `
    + `${external.length} external, ${target.length} target, ${forCodeCases.length} forCode, ${bridge.length} bridge `
    + `(${bridge.filter((b) => b.status === 200).length} answered, ${bridge.filter((b) => b.status === 503).length} unavailable)`;
  const at = process.argv.indexOf('--check');
  if (at > 0) {
    const committed = fs.readFileSync(process.argv[at + 1], 'utf8');
    if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
    process.stderr.write(`ok: ${counts} recorded as committed\n`);
  } else {
    process.stderr.write(`recorded: ${counts}\n`);
    process.stdout.write(text);
  }
})().catch((e) => { process.stderr.write(`${e?.stack || e}\n`); process.exit(1); });
