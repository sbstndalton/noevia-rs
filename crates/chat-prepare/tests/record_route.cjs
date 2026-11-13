#!/usr/bin/env node
'use strict';
// Recorded reasoning effort, Fast thinking-off and sampling presets for crates/chat-prepare (slice P5a): the contract
// src/effort.rs, src/sampling.rs and src/route.rs are replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-prepare/tests/record_route.cjs > crates/chat-prepare/tests/fixtures/node-route.v1.json
//   ... node crates/chat-prepare/tests/record_route.cjs --check crates/chat-prepare/tests/fixtures/node-route.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Nothing from node_modules and no dav-parse.wasm. Every expectation comes from noevia-core's own code; the inputs are
// synthetic and QA-named (hand-made edges plus a seeded PRNG), passed through JSON first so Node reads exactly what is
// recorded (a field left out of a case is `undefined`):
//   efforts      reasoning-effort.cjs validEffort(value)
//   resolve      resolveEffort(project, globalDefault)
//   forRoute     effortForRoute(effort, routedRole)
//   thinkingOff  fastRouteThinkingOff({ auto, routedRole, projectEffort })
//   engines      localEngine(provider) under process.env MODEL_MANAGER_KIND and INFERENCE_BASE_URL
//   values       sampling-presets.cjs validSamplingValue(key, value)
//   sanitize     sanitizeExplicitSampling(raw)
//   classify     classifyTaskForSampling({ routedRole, message }) of each message for every role of `roles` (in order;
//                its nulls are null and undefined)
//   creative     creativeRoute({ routedRole, message, roles })
//   select       selectSamplingParams({ routedRole, message, explicit, autoEnabled })
//   turns        chat.cjs's own lines from `const resolvedEffort =` to the selectSamplingParams call (cut out of the
//                pinned chat.cjs and run as they are, over a stub settings table), and what the turn sends of them:
//                `request` = { ...sampling.params, ...thinkingOffFields } (the model request's spread), `meta` = the
//                meta event's `sampling`, `prepared` = the native executor's effort, thinkingOff and sampling
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
// Read besides the modules: chat.cjs, whose lines are run below without loading it.
const READ_FILES = ['server/chat.cjs'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex');
function sourcesRead() {
  const server = `${path.join(root, 'server')}${path.sep}`;
  const loaded = Object.keys(require.cache).filter((k) => k.startsWith(server)).map((k) => path.relative(root, k).split(path.sep).join('/'));
  if (loaded.some((k) => k.includes('node_modules/'))) throw new Error('a module from node_modules was loaded: the header says nothing is');
  return Object.fromEntries([...new Set([...READ_FILES, ...loaded])].sort().map((f) => [f, sha(f)]));
}
const serverRequire = (f) => require(path.join(root, 'server', f));
const effortLib = serverRequire('reasoning-effort.cjs');
const presets = serverRequire('sampling-presets.cjs');

// ── chat.cjs's block ────────────────────────────────────────────────────────────────────────
const chatText = fs.readFileSync(path.join(root, 'server/chat.cjs'), 'utf8');
const START = '    const resolvedEffort = reasoningEffort.resolveEffort(project, ';
const END = '      routedRole, message, explicit: project?.sampling, autoEnabled: samplingAutoEnabled,\n    });\n';
const once = (needle) => {
  const at = chatText.indexOf(needle);
  if (at < 0 || chatText.indexOf(needle, at + 1) >= 0) throw new Error(`chat.cjs holds ${JSON.stringify(needle.slice(0, 60))} ${at < 0 ? 'nowhere' : 'twice'}`);
  return at;
};
const blockStart = once(START);
const blockEnd = once(END) + END.length;
const BLOCK = chatText.slice(blockStart, blockEnd);
const blockLines = [chatText.slice(0, blockStart).split('\n').length, chatText.slice(0, blockEnd).split('\n').length - 1];
// What the turn does with the block's values, as these lines of chat.cjs write it (checked to be there).
for (const use of [
  "sampling: sampling.source === 'none' ? undefined : { preset: sampling.presetId || undefined, source: sampling.source, values: sampling.params } });",
  'effort, thinkingOff: fastThinkingOff, sampling: sampling.params,',
  '...sampling.params, ...thinkingOffFields,',
]) once(use);
const DEFAULT_PROVIDER_ID = 'default'; // index.cjs's, with the DEFAULT_PROVIDER_ID variable unset
const runBlock = new Function('project', 'authService', 'wantsAuto', 'routedRole', 'provider', 'DEFAULT_PROVIDER_ID', 'reasoningEffort', 'message', 'require',
  `${BLOCK}\nreturn { resolvedEffort, effort, fastThinkingOff, thinkingOffFields, samplingAutoEnabled, sampling };`);

// ── Deterministic inputs ────────────────────────────────────────────────────────────────────
let seed = 0x2026_1113;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const pick = (list) => list[Math.floor(rand() * list.length)];
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));
const setEnv = (key, value) => { if (value === undefined) delete process.env[key]; else process.env[key] = value; };

const EFFORT_VALUES = ['default', 'low', 'high', 'Default', 'LOW', 'High', 'HIGH', ' low', 'low ', '\tlow', 'high\n', 'low\u0000', '',
  'none', 'medium', 'auto', 'minimal', 'lo', 'l\u043ew', '\ufeffhigh', 'ｌｏｗ', '\ud83d', 0, 1, -0, 2.5, null, true, false, {}, [], ['low'],
  { low: true }, { toString: 'low' }, undefined];
const ROLES = ['fast', 'smart', 'code', 'creative', '', 'Fast', 'FAST', ' fast', 'fast ', 'smart\n', 'qa-unknown', 'vision', null, undefined];

// ── reasoning-effort.cjs ────────────────────────────────────────────────────────────────────
const efforts = EFFORT_VALUES.map((value) => { value = viaJson(value); return { value, out: effortLib.validEffort(value) }; });
const resolve = [];
const PROJECTS = [null, undefined, {}, { routing: 'auto' }, ...EFFORT_VALUES.map((reasoningEffort) => ({ reasoningEffort })), 'qa-project', 5, []];
const DEFAULTS = [undefined, null, 'default', 'low', 'high', 'LOW', '', ' high', 'medium', 5, true, {}];
for (const p of PROJECTS) {
  for (const d of DEFAULTS) {
    const project = viaJson(p), globalDefault = viaJson(d);
    resolve.push({ project, globalDefault, out: effortLib.resolveEffort(project, globalDefault) });
  }
}
const forRoute = [];
for (const effort of ['default', 'low', 'high']) for (const r of ROLES) forRoute.push({ effort, routedRole: viaJson(r), out: effortLib.effortForRoute(effort, viaJson(r)) });
const thinkingOff = [];
for (const auto of [true, false]) {
  for (const r of ROLES) {
    for (const e of EFFORT_VALUES) {
      const routedRole = viaJson(r), projectEffort = viaJson(e);
      thinkingOff.push({ auto, routedRole, projectEffort, out: effortLib.fastRouteThinkingOff({ auto, routedRole, projectEffort }) });
    }
  }
}

// localEngine: the provider's base URL and INFERENCE_BASE_URL through `new URL(value).href.replace(/\/+$/, '')`.
const ENGINE = 'http://qa-engine.invalid:8080/v1';
const URLS = [ENGINE, 'http://qa-engine.invalid:8080', 'http://qa-engine.invalid:8080/', 'http://qa-engine.invalid:8080//',
  'http://qa-engine.invalid:8080/v1/', 'http://qa-engine.invalid:8080/v1//', 'HTTP://QA-Engine.INVALID:8080/v1', 'http://qa-engine.invalid:8080/V1',
  'http://qa-engine.invalid:80/v1', 'http://qa-engine.invalid/v1', 'https://qa-engine.invalid:443/v1', 'https://qa-engine.invalid/v1',
  'http://qa-engine.invalid:08080/v1', ` ${ENGINE} `, `\t${ENGINE}\n`, 'http://qa-en\tgine.invalid:8080/v\n1', 'http:\\\\qa-engine.invalid:8080\\v1',
  'http:qa-engine.invalid:8080/v1', 'http:///qa-engine.invalid:8080/v1', 'http://qa-engine.invalid:8080/x/../v1', 'http://qa-engine.invalid:8080/./v1',
  'http://qa-engine.invalid:8080/%2e/v1', 'http://qa-engine.invalid:8080/%76%31', 'http://qa-engine.invalid:8080/v1?', 'http://qa-engine.invalid:8080/v1#',
  'http://qa-engine.invalid:8080/v1?x=/', 'http://qa-engine.invalid:8080/v1#/', 'http://qa-engine.invalid:8080/v1?q=\'a b\'',
  'http://user@qa-engine.invalid:8080/v1', 'http://:@qa-engine.invalid:8080/v1', 'http://qa-engine.invalid.:8080/v1',
  'http://127.0.0.1:8080/v1', 'http://0x7f.0.0.1:8080/v1', 'http://2130706433:8080/v1', 'http://127.1:8080/v1', 'http://0177.0.0.1:8080/v1',
  'http://[::1]:8080/v1', 'http://[0:0:0:0:0:0:0:1]:8080/v1', 'http://[::ffff:127.0.0.1]:8080/v1', 'http://localhost:8080/v1', 'http://LOCALHOST:8080/v1',
  'http://bücher.invalid:8080/v1', 'http://xn--bcher-kva.invalid:8080/v1', 'http://BÜCHER.invalid:8080/v1', 'http://qa-engine。invalid:8080/v1',
  'http://ｑａ-engine.invalid:8080/v1', 'http://qa\u00ad-engine.invalid:8080/v1', 'http://%71a-engine.invalid:8080/v1',
  'http://qa-engine.invalid:8080/v1/ü', 'http://qa-engine.invalid:8080/v1/%C3%BC', 'http://qa-engine.invalid:8080/v1/\ud800',
  'http://qa-engine.invalid:8080/v1/\ufffd', 'http://qa-engine.invalid:8080/v1 /', 'qa://qa-engine.invalid:8080/v1', 'QA://qa-engine.invalid:8080/v1',
  'qa://qa-engine.invalid:8080/v1/', 'qa-engine.invalid:8080', 'localhost:8080', 'file:///qa/v1', 'file:///C:/qa', 'file:///c|/qa', '', ' ', 'not a url',
  'http://', 'http://qa engine.invalid', 'http://qa-engine.invalid:65536/v1', 'http://[::1', 'http://1.2.3.4.5/', 'http://999.0.0.1/',
  'http://0x100000000/', 'blob:http://qa-engine.invalid/x', 'http://qa_engine.invalid:8080/v1'];
const KINDS = [undefined, 'llamacpp', 'lemonade', 'LlamaCpp', 'LEMONADE', 'llamacpp ', 'none', 'ollama', ''];
const engines = [];
const addEngine = (kind, inference, rawProvider) => {
  const provider = viaJson(rawProvider);
  setEnv('MODEL_MANAGER_KIND', kind);
  setEnv('INFERENCE_BASE_URL', inference);
  // What process.env holds (a lone surrogate reads back as U+FFFD, a NUL ends the value).
  engines.push({ kind: process.env.MODEL_MANAGER_KIND, inference: process.env.INFERENCE_BASE_URL, provider, out: effortLib.localEngine(provider) });
};
for (const a of URLS) for (const b of URLS) addEngine('llamacpp', b, { id: 'default', baseUrl: a });
// Only as the row's URL: an environment value cannot hold a NUL.
for (const a of ['http://qa-engine.invalid:8080/v1\u0000', 'http://qa-engine.invalid:8080/v1\u0000/', '\u0000http://qa-engine.invalid:8080/v1']) {
  addEngine('llamacpp', ENGINE, { id: 'default', baseUrl: a });
}
for (const kind of KINDS) {
  for (const [baseUrl, inference] of [[ENGINE, ENGINE], [`${ENGINE}/`, ENGINE], [ENGINE, 'http://qa-other.invalid/v1'], [undefined, ENGINE],
    [ENGINE, undefined], ['', ''], ['not a url', 'not a url']]) {
    addEngine(kind, inference, { id: 'default', baseUrl });
  }
}
for (const baseUrl of [undefined, null, 5, true, [ENGINE], [[`${ENGINE}/`]], ['http://qa-engine.invalid:8080', 'v1'], [null], [{}], {},
  { toString: ENGINE }, { valueOf: ENGINE }, [{ toString: ENGINE }], [ENGINE, null]]) {
  for (const inference of [ENGINE, '[object Object]', 'http://qa-engine.invalid:8080/v1,']) addEngine('lemonade', inference, { id: 'default', baseUrl });
}
for (const provider of [null, 'qa', 5, [], { id: 'default' }, { baseUrl: ENGINE }, { id: 'machine-qa-1', baseUrl: ENGINE, machine: { id: 'qa-1' } }]) {
  addEngine('llamacpp', ENGINE, provider);
}

// ── sampling-presets.cjs ────────────────────────────────────────────────────────────────────
const KEYS = [...presets.SAMPLING_KEYS, 'min_p', 'Temperature', 'topP', '', 'toString'];
const VALUES = [0, -0, 0.5, 1, 1.0000001, 0.9999999, 2, 2.0000001, 3, 3.0000000000000004, 3.5, -1, -0.0001, 1e-300, 5e-324, 1e308, 7, 7.5, 40,
  2 ** 53, 2 ** 53 + 2, 1e21, '0.5', '', null, true, false, [], [0.5], {}, { value: 0.5 }, undefined];
const values = [];
for (const key of KEYS) for (const v of VALUES) { const value = viaJson(v); values.push({ key, value, out: presets.validSamplingValue(key, value) }); }
const EXPLICIT = [undefined, null, 0, 1, '', 'qa', true, false, [], [0.5], ['temperature'], {}, { temperature: 0.7 }, { top_p: 0.5 }, { top_k: 40 },
  { repeat_penalty: 1.1 }, { temperature: 0.2, top_p: 0.9, repeat_penalty: 1.05 }, { temperature: 0.9, top_p: 0.95 },
  { repeat_penalty: 1.2, top_k: 20, top_p: 0.8, temperature: 0.3 }, { top_k: 5, temperature: 0 }, { temperature: -0, top_k: -0 },
  { temperature: '0.7', top_p: 2, top_k: 4.5, repeat_penalty: -1 }, { temperature: null, top_p: [], top_k: {}, repeat_penalty: true },
  { temperature: 2.5, top_p: 0, top_k: -1, repeat_penalty: 3.5 }, { temperature: 0.7, min_p: 0.1, seed: 7, Temperature: 0.3 },
  { 0: 0.5, temperature: 1.5, 1: 'qa' }, { top_p: 1, repeat_penalty: 3 }, { temperature: 0.5, extra: { temperature: 1 } },
  { sampling: { temperature: 0.4 } }, { top_k: 1e21 }, { top_k: 2 ** 53 + 2, top_p: 5e-324 }];
const sanitize = EXPLICIT.map((r) => { const raw = viaJson(r); return { raw, out: presets.sanitizeExplicitSampling(raw) }; });

// Messages: every branch of CREATIVE_RE (each verb, article, `short`, each noun, the two other alternatives), its word
// boundaries and whitespace (JS \s: BOM and U+3000 are space, NEL and ZWSP are not), ASCII-only case folding (ſ, the
// Kelvin sign and dotless i do not fold to ASCII without /u), code-ish, factual, empty, astral, lone surrogates, long and
// mixed-script text.
const MESSAGES = ['', ' ', '\n\t', 'hi', 'Say hello', 'What is the capital of the QA region?', 'How many QA tickets closed in 2026?',
  'write a poem', 'Write a short story about a QA tester', 'WRITE AN ODE', 'write an epic poem', 'write haiku', 'write short poem',
  'write some short story', 'compose a song', 'Compose some lyrics for QA', 'draft a screenplay', 'draft an sonnet', 'draft some fiction',
  'write a novel', 'write a scene', 'write poetry', 'write a  short\t\tstory', 'write\na\npoem', 'write a\u00a0poem', 'write a\u0085poem',
  'write a\ufeffpoem', 'write a\u2028poem', 'write a\u3000poem', 'write a\u200bpoem', 'write a\u1680poem', 'write a\u180epoem',
  'write ashort story', 'write anovel', 'write astory', 'writea poem', 'rewrite a poem', 'write a poems', 'write a poem.', 'write a poem_',
  'write a poem1', 'write a poemé', 'éwrite a poem', '_write a poem', 'write a ſtory', 'write a hai\u212au', 'write some lyr\u0131cs',
  'wr\u0130te a poem', 'ｗｒｉｔｅ a poem', 'creative writing', 'Creative   Writing tips please', 'creativewriting', 'creative writings',
  'brainstorm names', 'Brainstorm TITLES for the QA board', 'brainstorm taglines', 'brainstorm ideas', 'brainstorm  names', 'brainstormnames',
  '😀write a poem😀', '𝐰𝐫𝐢𝐭𝐞 a poem', 'write a 😀 poem', '\ud83dwrite a poem', 'write a poem\ude00', 'write a \ud83d poem',
  'function qa() { return 1; }', 'write a function that sorts QA tickets', "```js\nconsole.log('qa')\n```", 'write a story in python',
  'fix the bug in qa.rs: let x = y?;', 'SELECT * FROM qa_table WHERE id = 1;', 'Напиши write a poem 日本語', 'اكتب write a song',
  'write a 詩', '日本語で詩を書いて', 'écris un poème', 'Schreibe ein Gedicht', `${'qa '.repeat(20000)}write a poem`, `${'qa '.repeat(20000)}`,
  `write ${'a'.repeat(5000)} poem`, `write ${' '.repeat(5000)}poem`];
const SELECT_MESSAGES = ['', 'Say hello', 'function qa() { return 1; }', 'Write a short story about a QA tester', 'brainstorm names',
  '😀write a poem😀', `${'qa '.repeat(300)}creative writing`, 'Напиши write a song 日本語'];
// Generated messages from the regex's own words and the separators that decide it.
const WORDS = ['write', 'Write', 'WRITE', 'compose', 'draft', 'rewrite', 'a', 'an', 'some', 'short', 'Short', 'story', 'poem', 'poetry', 'poems',
  'song', 'lyrics', 'screenplay', 'scene', 'fiction', 'novel', 'haiku', 'sonnet', 'creative', 'writing', 'brainstorm', 'names', 'titles', 'taglines',
  'ideas', 'qa', 'code', 'the'];
const SEPS = [' ', ' ', ' ', '  ', '\t', '\n', '\u00a0', '\u0085', '\ufeff', '\u200b', '\u3000', '\u2028', '', '_', '-', '.', 'é', '😀', '\ud83d', '1'];
for (let n = 0; n < 500; n += 1) {
  let text = rand() < 0.3 ? pick(SEPS) : '';
  for (let i = int(1, 6); i > 0; i -= 1) text += pick(WORDS) + (i > 1 ? pick(SEPS) : '');
  if (rand() < 0.3) text += pick(SEPS);
  MESSAGES.push(text);
}
const classify = MESSAGES.map((message) => ({ message, presets: ROLES.map((routedRole) => presets.classifyTaskForSampling({ routedRole, message })) }));
classify.push({ presets: ROLES.map((routedRole) => presets.classifyTaskForSampling({ routedRole })) });
const creative = [];
const ROLE_SETS = [null, undefined, 'qa', 5, [], {}, { creative: '' }, { creative: 'qa-creative' }, { creative: 5 }, { creative: ['qa'] },
  { fast: 'qa-fast', smart: 'qa-smart' }, { fast: 'qa-fast', creative: 'qa-creative' }];
for (const message of [...SELECT_MESSAGES, 'write a poem', 'compose a song in rust', 'rewrite a poem', undefined]) {
  for (const r of [null, 'fast', 'smart', 'code', 'creative', 'qa-unknown', undefined]) {
    for (const rs of ROLE_SETS) {
      const roles = viaJson(rs), routedRole = viaJson(r);
      creative.push({ routedRole, message, roles, out: presets.creativeRoute({ routedRole, message, roles }) });
    }
  }
}
const select = [];
const addSelect = (args) => {
  const input = viaJson(args);
  select.push({ ...input, out: presets.selectSamplingParams(input) });
};
for (const message of SELECT_MESSAGES) {
  for (const routedRole of [null, 'fast', 'smart', 'code', 'creative', 'qa-unknown']) {
    for (const explicit of EXPLICIT) for (const autoEnabled of [true, false]) addSelect({ routedRole, message, explicit, autoEnabled });
  }
}
addSelect({});
addSelect({ autoEnabled: false });
addSelect({ routedRole: 'code' });
addSelect({ message: 'write a poem' });
addSelect({ explicit: { temperature: 0.4 } });

// ── The turn: chat.cjs's block ──────────────────────────────────────────────────────────────
const turns = [];
const LOCAL = { id: 'default', label: 'QA engine', baseUrl: ENGINE };
const PROVIDERS = [LOCAL, { id: 'default', baseUrl: `${ENGINE}/` }, { id: 'default', baseUrl: 'http://qa-other.invalid/v1' },
  { id: 'qa-cloud', baseUrl: ENGINE }, { id: 'qa-cloud', baseUrl: 'https://qa-cloud.invalid/v1', capabilities: { reasoningEffortParam: true } },
  { id: 'machine:qa-1', baseUrl: 'http://qa-machine.invalid:8080', machine: { id: 'qa-1', thinkingSwitch: true } },
  { id: 'machine:qa-2', baseUrl: 'http://qa-machine.invalid:8081', machine: { id: 'qa-2' } }, { id: 'default', baseUrl: ENGINE, machine: { id: 'qa-1' } },
  { id: 'qa-cloud', baseUrl: 'https://qa-cloud.invalid/v1', machine: false }, { id: 'qa-cloud', machine: '' }, { id: 'qa-cloud', machine: 0 },
  { id: 'qa-cloud', machine: [] }, { id: 'qa-cloud', machine: 'qa' }, { id: 'Default', baseUrl: ENGINE }];
const ENVS = [['llamacpp', ENGINE], ['lemonade', `${ENGINE}/`], [undefined, ENGINE], ['none', ENGINE], ['llamacpp', undefined]];
const SETTING_EFFORT = [undefined, null, 'default', 'low', 'high', 'High', 'bogus', '', 5];
const SETTING_SAMPLING = [undefined, null, 'true', 'false', 'FALSE', 'False', '0', '', ' false', 'false ', 0, 1];
const TURN_PROJECTS = [null, {}, { routing: 'auto' }, { reasoningEffort: 'low' }, { reasoningEffort: 'high' }, { reasoningEffort: 'default' },
  { reasoningEffort: 'LOW' }, { reasoningEffort: 5 }, { reasoningEffort: null }, { sampling: { temperature: 0.7 } },
  { sampling: { temperature: 0.2, top_p: 0.9, repeat_penalty: 1.05 } }, { sampling: { top_k: 40 } }, { sampling: 'qa' },
  { sampling: { temperature: '0.7', top_p: 2 } }, { reasoningEffort: 'high', sampling: { temperature: 0, top_p: 1, top_k: 0, repeat_penalty: 0 } },
  { reasoningEffort: 'low', sampling: { repeat_penalty: 1.3, temperature: 0.1 } }, { reasoningEffort: 'medium', sampling: null }];
const TURN_ROLES = [null, 'fast', 'smart', 'code', 'creative', 'qa-unknown'];
function addTurn(raw) {
  const t = viaJson(raw);
  setEnv('MODEL_MANAGER_KIND', t.kind);
  setEnv('INFERENCE_BASE_URL', t.inference);
  t.kind = process.env.MODEL_MANAGER_KIND;
  t.inference = process.env.INFERENCE_BASE_URL;
  let authService;
  if (t.settings === null) authService = { db: null };
  else if (t.settings !== undefined) {
    const rows = t.settings;
    authService = { db: { prepare(sql) {
      const key = /key='([a-z_]+)'/.exec(sql)[1];
      return { get: () => (Object.prototype.hasOwnProperty.call(rows, key) ? { value: rows[key] } : undefined) };
    } } };
  }
  const out = runBlock(t.project, authService, t.wantsAuto, t.routedRole, t.provider, DEFAULT_PROVIDER_ID, effortLib, t.message, serverRequire);
  const { effort, fastThinkingOff, thinkingOffFields, sampling } = out;
  turns.push({ ...t, out: { ...out,
    request: { ...sampling.params, ...thinkingOffFields },
    meta: sampling.source === 'none' ? undefined : { preset: sampling.presetId || undefined, source: sampling.source, values: sampling.params },
    prepared: { effort, thinkingOff: fastThinkingOff, sampling: sampling.params } } });
}
const settingsOf = (e, s) => {
  const rows = {};
  if (e !== undefined) rows.reasoning_effort_default = e;
  if (s !== undefined) rows.auto_sampling_presets_enabled = s;
  return rows;
};
const turn = (extra) => ({ project: null, settings: {}, wantsAuto: true, routedRole: 'fast', provider: LOCAL, kind: 'llamacpp', inference: ENGINE,
  message: 'Say hello', ...extra });
// Effort: auto × role × the project's effort × the account default, on the local engine.
for (const wantsAuto of [true, false]) {
  for (const routedRole of TURN_ROLES) {
    for (const project of [null, { reasoningEffort: 'low' }, { reasoningEffort: 'high' }, { reasoningEffort: 'default' }, { reasoningEffort: 'LOW' }]) {
      for (const e of [undefined, 'high', 'low', 'default', 'bogus']) addTurn(turn({ wantsAuto, routedRole, project, settings: settingsOf(e) }));
    }
  }
}
// Thinking off: the provider row and the engine's environment.
for (const wantsAuto of [true, false]) {
  for (const provider of PROVIDERS) for (const [kind, inference] of ENVS) addTurn(turn({ wantsAuto, provider, kind, inference }));
}
// No settings table at all, and one without the rows.
for (const settings of [undefined, null, {}]) for (const routedRole of TURN_ROLES) addTurn(turn({ settings, routedRole }));
// Sampling: the setting × the project's sampling × the role × the message.
for (const s of SETTING_SAMPLING) {
  for (const project of TURN_PROJECTS) {
    for (const routedRole of ['fast', 'code', 'smart']) addTurn(turn({ settings: settingsOf(undefined, s), project, routedRole, message: 'Write a short story about a QA tester' }));
  }
}
for (const message of SELECT_MESSAGES) for (const routedRole of TURN_ROLES) addTurn(turn({ routedRole, message }));
// A generated corpus over everything at once.
const SHORT_MESSAGES = MESSAGES.filter((m) => m.length < 200);
for (let n = 0; n < 600; n += 1) {
  const [kind, inference] = pick(ENVS);
  const settings = rand() < 0.05 ? pick([undefined, null]) : settingsOf(pick(SETTING_EFFORT), pick(SETTING_SAMPLING));
  addTurn({ project: pick(TURN_PROJECTS), settings, wantsAuto: rand() < 0.7, routedRole: pick(TURN_ROLES), provider: pick(PROVIDERS), kind, inference,
    message: pick(SHORT_MESSAGES) });
}
setEnv('MODEL_MANAGER_KIND', undefined);
setEnv('INFERENCE_BASE_URL', undefined);

// Named differences: none found (every case above replays byte for byte).
const differences = [];

// ── Output ──────────────────────────────────────────────────────────────────────────────────
const sources = sourcesRead();
const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
const text = `{"version":1,"generator":"crates/chat-prepare/tests/record_route.cjs",\n"node":${JSON.stringify(process.version)},\n`
  + `"sources":${JSON.stringify(sources)},\n"block":${JSON.stringify({ lines: blockLines, sha256: crypto.createHash('sha256').update(BLOCK).digest('hex') })},\n`
  + `"defaultProviderId":${JSON.stringify(DEFAULT_PROVIDER_ID)},\n"presets":${JSON.stringify(presets.PRESETS)},\n"presetsVersion":${presets.PRESETS_VERSION},\n`
  + `"samplingKeys":${JSON.stringify(presets.SAMPLING_KEYS)},\n"roles":${JSON.stringify(ROLES)},\n`
  + `"efforts":[\n${lines(efforts)}\n],\n"resolve":[\n${lines(resolve)}\n],\n"forRoute":[\n${lines(forRoute)}\n],\n`
  + `"thinkingOff":[\n${lines(thinkingOff)}\n],\n"engines":[\n${lines(engines)}\n],\n"values":[\n${lines(values)}\n],\n`
  + `"sanitize":[\n${lines(sanitize)}\n],\n"classify":[\n${lines(classify)}\n],\n"creative":[\n${lines(creative)}\n],\n`
  + `"select":[\n${lines(select)}\n],\n"turns":[\n${lines(turns)}\n],\n"differences":[\n${lines(differences)}\n]}\n`;
const check = process.argv.indexOf('--check');
const local = engines.filter((c) => c.out).length;
const off = turns.filter((c) => c.out.fastThinkingOff).length;
const counts = `${efforts.length} efforts, ${resolve.length} resolve, ${forRoute.length} forRoute, ${thinkingOff.length} thinkingOff, `
  + `${engines.length} engines (${local} local), ${values.length} values, ${sanitize.length} sanitize, ${classify.length} classify, `
  + `${creative.length} creative, ${select.length} select, ${turns.length} turns (${off} thinking off), ${differences.length} differences`;
if (check > 0) {
  const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
  if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
  process.stderr.write(`ok: ${counts} recorded as committed\n`);
} else {
  process.stderr.write(`recorded: ${counts}\n`);
  process.stdout.write(text);
}
