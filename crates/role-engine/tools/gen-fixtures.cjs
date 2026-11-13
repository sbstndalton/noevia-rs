'use strict';
// Records tests/fixtures/role-engine.v1.json from noevia-core's own role-engine.cjs:
//
//   node crates/role-engine/tools/gen-fixtures.cjs <noevia-core>/server > crates/role-engine/tests/fixtures/role-engine.v1.json
//
// The checkout must have server/wasm/dav-parse.wasm at its pinned sha: the stream guard and the
// role-context projections answer through the Rust crates inside that module. The sha256 of every
// source file the engine reads goes into the fixture (checked by a Rust const).
//
// Values are written in a tagged JSON form so that `undefined`, NaN and the infinities survive:
// {"$js":"undefined"|"NaN"|"Infinity"|"-Infinity"|"-0"}. Inputs are plain JSON.
//
// Two parts: `pure` (one row per call of an exported pure function) and `scenarios`
// (createRoleEngine against scripted stubs: every result, every request the stubs saw, every
// dependency call in order, every log entry and the session timings). The stub clock starts at
// 1000 and adds 5 on every read, so timing fields are exact.
const path = require('node:path');
const fs = require('node:fs');
const crypto = require('node:crypto');

const dir = process.argv[2];
if (!dir) { process.stderr.write('usage: gen-fixtures.cjs <noevia-core>/server\n'); process.exit(2); }
const RE = require(path.resolve(dir, 'role-engine.cjs'));
const MS = require(path.resolve(dir, 'model-system.cjs'));
console.warn = () => {};

const TAG = '$js';
function enc(v) {
  if (v === undefined) return { [TAG]: 'undefined' };
  if (typeof v === 'number') {
    if (Number.isNaN(v)) return { [TAG]: 'NaN' };
    if (v === Infinity) return { [TAG]: 'Infinity' };
    if (v === -Infinity) return { [TAG]: '-Infinity' };
    if (Object.is(v, -0)) return { [TAG]: '-0' };
    return v;
  }
  if (v === null || typeof v !== 'object') return v;
  if (Array.isArray(v)) return Array.from(v, enc);
  const out = {};
  for (const k of Object.keys(v)) out[k] = enc(v[k]);
  return out;
}
const result = (fn) => { try { return { out: enc(fn()) }; } catch (e) { return { throws: String(e && e.message) }; } };

// ── Pure functions ───────────────────────────────────────────────────────────────────────────
const pure = [];
const row = (fn, args, value) => pure.push({ fn, args, ...value });

const SCHEMA = { type: 'object', additionalProperties: false, required: ['answer'], properties: { answer: { type: 'string' } } };
const YES = { capabilities: { jsonSchemaParam: true } };
for (const enabled of [false, true]) for (const provider of [null, {}, YES, { capabilities: { jsonSchemaParam: 'true' } }, 'x', [1]])
  for (const model of ['', 'qa-coder-q4', 'gpt-oss-20b', 'QA/GPT_OSS-120B.gguf', 'xgpt-oss', 'gpt-oss2', 'gpt-ossy', 'my gpt.oss', 'gptoss', 'qwen3-gpt-oss'])
    for (const thinking of [false, true]) {
      if (!enabled && (provider !== null || model !== '')) continue;
      for (const name of ['role_answer', 'qa_name']) {
        if (name !== 'role_answer' && (thinking || model !== 'qa-coder-q4')) continue;
        const args = { enabled, provider, model, thinking, schema: SCHEMA, name };
        row('schemaConstraint', args, result(() => RE.schemaConstraint(args)));
      }
    }

const TEXTS = ['', 'QA dossier {"a":1}', 'é ü 😀 "quoted" \\ back\nline\ttab \u0001 \u2028', '\ud800 lone \udfff', '\u007f\u0085\ufeff'];
for (const dossier of TEXTS) for (const instructions of ['Act as planner.', TEXTS[2]]) for (const fields of ['{}', TEXTS[3]])
  for (const correction of [null, { violation: { message: 'Unexpected key "x"', path: '$.x' } }, { violation: { message: 'm \ud83d', path: null } }, {}]) {
    if (dossier !== TEXTS[1] && (instructions !== 'Act as planner.' || fields !== '{}') && correction) continue;
    const args = { dossier, instructions, fields, correction };
    row('buildMessages', args, result(() => RE.buildMessages(args)));
  }
for (const model of ['qa-coder-q4', 'é😀', '\udc00']) for (const thinking of [false, true]) for (const dossier of TEXTS)
  row('prefixHash', { model, thinking, dossier }, result(() => RE.prefixHash({ model, thinking, dossier })));

const ROWS = [
  { id: 'qa-coder-q4', status: { value: 'loaded', args: ['--model', '/m/qa-coder-q4.gguf'] } },
  { id: 'qa-other', status: { value: 'loading' } },
  { id: 'qa-off', status: { value: 'unloaded' } },
  { id: 'qa-str', status: 'loaded' },
  { id: 'qa-nostatus' },
  { id: 'qa-embed', status: { value: 'loaded', args: ['--model', '/m/e.gguf', '--embedding'] } },
  { id: 'qa-embeds', status: { value: 'loaded', args: ['--embeddings'] } },
  { id: 'qa-rr', status: { value: 'loaded', args: ['--reranking'] } },
  { id: 'qa-rr2', status: { value: 'loaded', args: ['--rerank'] } },
  { id: 'qa-label', labels: ['Reranker'], status: { value: 'loaded' } },
  { id: 'qa-label2', labels: [null, 'EMBEDDINGS'], status: { value: 'loaded' } },
  { id: 'qa-label3', labels: ['embedding model'], status: { value: 'loaded' } },
  { id: 'qa-label4', labels: 'rerank', status: { value: 'loaded' } },
  { id: 'Laya_multi', status: { value: 'loaded' } },
  { id: ' laya ', status: { value: 'loaded' } },
  { id: 'layaX', status: { value: 'loaded' } },
  { id: 'qa-p1', status: { value: 'loaded', args: ['-m', 'C:\\models\\LAYA.f16.GGUF'] } },
  { id: 'qa-p2', status: { value: 'loaded', args: ['--model', '/m/laya-x/../qa.gguf'] } },
  { id: 'qa-p3', status: { value: 'loaded', args: ['--model', '/m/layax.gguf'] } },
  { id: 'qa-p4', status: { value: 'loaded', args: ['--model'] } },
  { id: 'qa-p5', status: { value: 'loaded', args: ['--model', 5, '-m', '/m/laya'] } },
  { id: 'qa-role', status: { value: 'loaded' } },
  { id: 'qa-role2', status: { value: 'loaded' } },
  { id: 'laya-chat', status: { value: 'loaded' } },
  { id: 5, status: { value: 'loaded' } },
  { id: '', status: { value: 'loaded' } },
  null, 'qa-x', { status: { value: 'loaded' } },
  { id: 'qa-upper', status: { value: 'LOADED' } },
];
const ROLE_TEXTS = ['', '{"qa-role":" System-Routing ","qa-role2":"chat","laya-chat":"chat","*":"laya","qa-p1":"LAYA"}', '[1]', 'not json'];
for (const roles of ROLE_TEXTS) {
  MS.rememberSectionRoles(roles);
  for (const keep of [[], ['qa-coder-q4', 'qa-other']]) {
    for (const body of [{ data: ROWS }, { data: 'x' }, null, [], { data: [] }])
      row('loadedFromModelList', { body, keep, roles }, result(() => RE.loadedFromModelList(body, keep)));
  }
  for (const m of [...ROWS, undefined, { id: 'qa', status: { args: 'x' } }, { id: { a: 1 }, status: {} }, { id: true }, { id: null, labels: [{}] }])
    row('isNonChatRow', { row: m === undefined ? null : m, roles }, result(() => RE.isNonChatRow(m === undefined ? null : m)));
  MS.rememberSectionRoles('');
}

// ── sseDeltas ───────────────────────────────────────────────────────────────────────────────
/** Script chunk: a string (UTF-8) or {b64} (raw bytes). */
const bytesOf = (c) => (typeof c === 'string' ? Buffer.from(c, 'utf8') : Buffer.from(c.b64, 'base64'));
function stubBody(chunks, req, onMarker) {
  let i = 0;
  const reader = {
    async read() {
      req.reads += 1;
      for (;;) {
        if (i >= chunks.length) return { done: true, value: undefined };
        const c = chunks[i++];
        if (c && c.abort) { onMarker(); continue; }
        return { done: false, value: new Uint8Array(bytesOf(c)) };
      }
    },
    async cancel() { req.cancelled = true; },
  };
  return { getReader: () => reader, cancel: async () => { req.cancelled = true; } };
}
const ev = (o) => `data: ${JSON.stringify(o)}\n\n`;
const delta = (content) => ev({ choices: [{ delta: { content } }] });
const reasoning = (t) => ev({ choices: [{ delta: { reasoning_content: t } }] });
const TIMINGS = { prompt_n: 812, cache_n: 790, prompt_ms: 41.5, predicted_n: 12, predicted_ms: 300.25 };
const final = (o = { timings: TIMINGS }) => ev({ choices: [{ delta: {}, finish_reason: 'stop' }], ...o }) + 'data: [DONE]\n\n';
const split = (s, n) => { const b = Buffer.from(s, 'utf8'); const out = []; for (let k = 0; k < b.length; k += n) out.push({ b64: b.subarray(k, k + n).toString('base64') }); return out; };

const SSE = [
  [delta('{"a'), delta('":1}'), final()],
  split(delta('é😀') + reasoning('think') + delta('x') + final(), 3),
  split(delta('é😀') + final(), 1),
  ['\ufeff' + delta('bom'), delta('two')],
  [{ b64: '77u/' }, delta('bom-only-first'), '\ufeff' + delta('second-bom')],
  ['data:{"choices":[{"delta":{"content":"tight"}}]}\r\n', 'data:   [DONE]   \r\n', delta('after')],
  ['event: x\n', ': comment\n', 'data: not json\n', 'data: {"choices":{"0":{"delta":{"content":"obj"}}}}\n', 'data: "str"\n', 'data: [1]\n', 'data: null\n', delta(''), reasoning(''), delta(5)],
  [delta('a'), ev({ error: { message: 'QA engine message' } }), delta('b')],
  [delta('a'), ev({ error: 0 }), ev({ error: '' }), delta('b')],
  [ev({ usage: { prompt_tokens: 9, completion_tokens: 3, prompt_tokens_details: { cached_tokens: 4 } } }), ev({ timings: { prompt_n: 1, cache_n: '2', prompt_ms: NaN, predicted_n: null } }), ev({ usage: { prompt_tokens: 100, completion_tokens: 1e400 } }), ev({ timings: [1], usage: 'x' })],
  [ev({ timings: { predicted_ms: 1, prompt_n: 2 }, usage: { prompt_tokens: 3, prompt_tokens_details: null } }), delta('z')],
  ['data: {"choices":[{"delta":{"content":"\\ud83d"}}]}\n', 'data: {"choices":[{"delta":{"content":"\\ude00!"}}]}\n'],
  [{ b64: Buffer.from([0x64, 0x61, 0x74, 0x61, 0x3a, 0x20, 0x22, 0xff, 0xc3, 0x22, 0x0a]).toString('base64') }, 'data: {"choices":[{"delta":{"content":"', { b64: 'gA==' }, '\u00a0"}}]}\n'],
  ['data: {"choices":[{"delta":{"content":"no newline"}}]}'],
  ['\u2028data: [DONE]\n', 'data:\u00a0[DONE]\u3000\n', 'data:\ufeff[DONE]\n', 'data: \u0085[DONE]\n'],
  [],
];
const sseJobs = [];
for (const chunks of SSE) for (const take of [Infinity, 1]) {
  const req = { reads: 0, cancelled: false };
  let clock = 1000;
  const now = () => (clock += 5);
  const m = { start: now(), bytes: 0, deltas: 0, reasoningDeltas: 0, firstDeltaMs: null, complete: false, timings: {} };
  const deltas = [];
  let error = null;
  const target = { fn: 'sseDeltas', args: { chunks, take: take === Infinity ? null : take } };
  pure.push(target);
  const p = (async () => {
    try {
      for await (const d of RE.sseDeltas({ body: stubBody(chunks, req, () => {}) }, m, now)) {
        deltas.push(d);
        if (deltas.length >= take) break;
      }
    } catch (e) { error = e.code || String(e.message); }
    target.out = enc({ deltas, error, m, reads: req.reads, cancelled: req.cancelled, clock });
  })();
  sseJobs.push(p);
}

// ── Scenarios ───────────────────────────────────────────────────────────────────────────────
const BASE = 'http://engine.invalid/v1/';
const MODEL = 'qa-coder-q4';
const OTHER = 'qa-chat-q8';
const E = (o = {}) => ({ baseUrl: BASE, apiKey: 'qa-key', model: MODEL, provider: null, ...o });
const SIDE = [
  { id: 'qa-embed', status: { value: 'loaded', args: ['--model', '/m/qa-embed.gguf', '--embedding'] } },
  { id: 'qa-rerank', status: { value: 'loaded', args: ['--model', '/m/qa-rerank.gguf'] } },
  { id: 'laya_multilingual_f16', status: { value: 'loaded', args: ['--model', '/m/laya_multilingual_f16.gguf'] } },
];
const list = (...loaded) => ({ status: 200, json: { data: [...[MODEL, OTHER].map((id) => ({ id, status: { value: loaded.includes(id) ? 'loaded' : 'unloaded', args: ['--model', `/m/${id}.gguf`] } })), ...SIDE] } });
const GOOD = JSON.stringify({ answer: 'QA answer é😀' });
const okStream = (text = GOOD, pieces = 3) => {
  const size = Math.ceil(text.length / pieces);
  const out = [];
  for (let k = 0; k < text.length; k += size) out.push(delta(text.slice(k, k + size)));
  return { status: 200, chunks: [...out.slice(0, -1), ...split(out[out.length - 1] + final(), 7)] };
};
const st = (extra = {}) => ({
  taskId: 'qa-task-1', tenantId: 'qa-tenant', revision: 1,
  request: 'Add a median helper to the QA repo.', projectInstructions: 'Keep functions small.',
  capabilities: [{ name: 'read' }, { name: 'edit' }],
  plan: { goal: 'median helper', steps: [{ do: 'write median', done_when: 'tests pass' }] },
  execution: { headSha: 'b'.repeat(40), changedFiles: ['median.js'], summary: 'wrote it' },
  change: { baseSha: 'a'.repeat(40), headSha: 'b'.repeat(40), files: [{ path: 'median.js', patch: '+module.exports = m;\n' }] },
  ...extra,
});
const C = (o = {}) => ({ op: 'call', role: 'planner', state: st(), instructions: 'Act as the QA planner.', schema: SCHEMA, ...o });
const P = (o = {}) => ({ op: 'pin', taskId: 'qa-task-1', ...o });
const MT = { machine: { id: 'qa-m1', name: 'QA Box' }, baseUrl: 'http://10.9.0.5:8080/v1', model: 'qa-remote-q4' };
const ME = (o = {}) => E({ machine: MT, ...o });
const READY = { ok: true };
const BAD = { status: 200, chunks: [delta('{"other":'), delta('1}'), final()] };
const ROUTER_CALL = [list(MODEL)];

const SCENARIOS = {
  happy: { fetch: [list(MODEL), list(MODEL), okStream(), list(MODEL), okStream(GOOD, 1)], steps: [P(), C(), C({ role: 'reviewer', instructions: 'Review it.' }), { op: 'timings' }] },
  cold_pin: { fetch: [list(), list(), okStream()], steps: [P(), C({ role: 'executor' })] },
  model_from_loaded: { engine: [E({ model: null, apiKey: 'local' })], fetch: [list(OTHER), list(OTHER), okStream()], steps: [P(), C()] },
  explicit_model: { fetch: [list(OTHER), list(OTHER), okStream()], steps: [P({ model: OTHER, thinking: true }), C({ constrain: true })] },
  no_model: { engine: [E({ model: '', apiKey: '' })], fetch: [list()], steps: [P()] },
  mismatch_pin: { fetch: [list(MODEL, OTHER)], steps: [P()] },
  keep_list: { keep: [OTHER, 7, 'qa-rerank'], fetch: [list(MODEL, OTHER), list(OTHER, MODEL), okStream()], steps: [P(), C()] },
  keep_not_array: { keep: 'notArray', fetch: [], steps: [P()] },
  mismatch_call: { fetch: [list(MODEL), list(OTHER)], steps: [P(), C(), { op: 'timings' }] },
  correction_ok: { fetch: [list(MODEL), list(MODEL), BAD, list(MODEL), okStream()], steps: [P(), C()] },
  correction_fails: { fetch: [list(MODEL), list(MODEL), BAD, list(MODEL), { status: 200, chunks: [delta('{"answer":'), delta('"x"'), final()] }], steps: [P(), C(), { op: 'timings' }] },
  constrained_fallback: { engine: [E({ provider: YES })], fetch: [list(MODEL), list(MODEL), { status: 400, chunks: ['QA engine detail'] }, okStream('{"x":1}'), list(MODEL), okStream()], steps: [P(), C({ constrain: true, schemaName: 'qa_schema' })] },
  constrained_ok: { engine: [E({ provider: YES })], fetch: [list(MODEL), list(MODEL), okStream()], steps: [P(), C({ constrain: true })] },
  constrained_500: { engine: [E({ provider: YES })], fetch: [list(MODEL), list(MODEL), { status: 500, chunks: ['x'] }], steps: [P(), C({ constrain: true })] },
  constrained_connect: { engine: [E({ provider: YES })], fetch: [list(MODEL), list(MODEL), { throw: 'connect' }], steps: [P(), C({ constrain: true })] },
  constrained_reasoning_model: { engine: [E({ provider: YES, model: 'qa-gpt-oss-20b' })], fetch: [list(), list(), okStream()], steps: [P(), C({ constrain: true })] },
  unconstrained_500: { fetch: [list(MODEL), list(MODEL), { status: 500, chunks: ['x'] }], steps: [P(), C()] },
  connect_error: { fetch: [list(MODEL), list(MODEL), { throw: 'connect' }], steps: [P(), C()] },
  unscripted: { fetch: [list(MODEL), list(MODEL)], steps: [P(), C()] },
  split_pair: { fetch: [list(MODEL), list(MODEL), { status: 200, chunks: ['data: {"choices":[{"delta":{"content":"{\\"answer\\":\\"\\ud83d"}}]}\n', 'data: {"choices":[{"delta":{"content":"\\ude00 \\udc00\\"}"}}]}\n', final()] }],
    steps: [P(), C({ state: st({ request: 'Ünïcödé 😀 request' }) }), C({ state: st({ projectInstructions: 'lone \ud800 here' }) })] },
  schema_null: { fetch: [list(MODEL), list(MODEL), okStream('[1, "x"]')], steps: [P(), C({ schema: null })] },
  budget_pin: { budget: [{ error: 'QA model needs 20 GB.' }], fetch: ROUTER_CALL, steps: [P()] },
  budget_pin_default: { budget: [{ error: '' }], fetch: ROUTER_CALL, steps: [P()] },
  budget_pin_throws: { budget: ['throw', { error: 'QA later' }], fetch: [list(MODEL)], steps: [P(), C()] },
  budget_call: { budget: [null, null, { error: 'QA budget changed.' }], fetch: [list(MODEL), list(MODEL), BAD], steps: [P(), C(), C()] },
  external: { engine: [E({ external: true })], steps: [P()] },
  unavailable: { engine: [E({ baseUrl: '' })], steps: [P()] },
  engine_throws: { engine: ['throw'], steps: [P()] },
  engine_changes: { engine: [E(), E({ external: true }), E({ baseUrl: null }), 'throw'], fetch: [list(MODEL)], steps: [P(), C(), C(), C()] },
  invalid_task: { steps: [P({ taskId: '' })] },
  task_and_role: { fetch: [list(MODEL)], steps: [P({ roles: ['planner'] }), C({ role: 'reviewer' }), C({ state: st({ taskId: 'qa-task-2' }) }), C({ state: null })] },
  abort_before: { fetch: [list(MODEL)], steps: [P({ abort: 'before' }), P(), C({ abort: 'before' })] },
  abort_mid_stream: { fetch: [list(MODEL), list(MODEL), { status: 200, chunks: [delta('{"answer"'), { abort: true }, delta(':"x"}'), final()] }], steps: [P(), C({ abort: 'armed' }), { op: 'timings' }] },
  abort_fetch: { fetch: [list(MODEL), list(MODEL), { abortThenThrow: true }], steps: [P(), C({ abort: 'armed' })] },
  abort_in_correction: { fetch: [list(MODEL), list(MODEL), BAD, list(MODEL), { abortThenThrow: true }], steps: [P(), C({ abort: 'armed' }), { op: 'timings' }] },
  abort_in_admission: { admission: ['abort'], fetch: [list(MODEL)], steps: [P({ abort: 'armed' })] },
  admission: { admission: ['pass', 'pass', 'pass'], fetch: [list(MODEL), list(MODEL), okStream()], steps: [P(), C()] },
  admission_throws: { admission: ['throw429', 'pass', 'throw'], fetch: [list(MODEL)], steps: [P(), P(), C()] },
  engine_error: { fetch: [list(MODEL), list(MODEL), { status: 200, chunks: [delta('{"ans'), ev({ error: { message: 'QA engine message' } }), delta('x')] }], steps: [P(), C(), { op: 'timings' }] },
  context_leak: { fetch: [list(MODEL)], steps: [P(), C({ role: 'reviewer', state: st({ request: 'see qa-tenant-b-secret-notes', otherTenants: { 'qa-tenant-b': ['qa-tenant-b-secret-notes'] } }) })] },
  context_invalid: { fetch: [list(MODEL)], steps: [P(), C({ state: { taskId: 'qa-task-1' } }), C({ state: { taskId: 'qa-task-1', tenantId: 5 } })] },
  router_503: { fetch: [{ status: 503, json: {} }], steps: [P()] },
  router_bad_json: { fetch: [{ status: 200, text: 'not json' }, list(MODEL), { status: 500, json: {} }], steps: [P(), P(), C()] },
  router_throws: { fetch: [{ throw: 'connect' }], steps: [P()] },
  loaded_models: { loadedModels: [[MODEL, 'qa-embed', 5, ''], [MODEL], 'notArray'], keep: ['qa-embed', 'qa-rerank'], fetch: [okStream()], steps: [P(), C(), C()] },
  loaded_models_other: { loadedModels: [[OTHER]], steps: [P()] },
  budgets: { roleDefaults: { planner: { maxTokens: 77 }, auditor: { maxBytes: 12 } }, fetch: [list(MODEL), list(MODEL), okStream(), list(MODEL), okStream(), list(MODEL), okStream(), list(MODEL), okStream('{"answer":"long text"}'), list(MODEL), okStream('{"answer":"long text"}')],
    steps: [P(), C(), C({ role: 'reviewer', maxTokens: 5, maxBytes: 0 }), C({ role: 'executor', maxTokens: 2.5 }), C({ role: 'auditor' })] },
  max_bytes_too_big: { fetch: [list(MODEL)], steps: [P(), C({ maxBytes: 4 * 1024 * 1024 })] },
  schema_not_plain: { fetch: [list(MODEL)], steps: [P(), C({ schema: { type: 'object', minLength: NaN } })] },
  stream_shapes: { fetch: [list(MODEL), list(MODEL), { status: 200, chunks: [reasoning('think'), reasoning('more'), ...split(delta('{"answer":"é😀"}'), 2), ev({ usage: { prompt_tokens: 9, completion_tokens: 3 } })] }, list(MODEL), { status: 200, chunks: [delta('{"answer":"x"'), final()] }, list(MODEL), { status: 200, chunks: [delta('{"answer":"y"}'), final(), delta('junk')] }],
    steps: [P(), C(), C(), { op: 'timings' }] },
  machine: { engine: [ME()], machines: { ready: [READY, READY] }, machineFetch: [okStream()], steps: [P(), C(), { op: 'timings' }] },
  machine_model_mismatch: { engine: [ME()], machines: { ready: [READY] }, steps: [P({ model: OTHER }), P({ model: 'qa-remote-q4', thinking: true })] },
  machine_mismatch_noname: { engine: [ME({ machine: { machine: {}, baseUrl: 'http://10.9.0.5:8080/v1', model: 'qa-remote-q4' } })], machines: { ready: [] }, steps: [P({ model: OTHER })] },
  machine_not_ready: { engine: [ME()], machines: { ready: [{ ok: false, code: 'machineWrongModel', error: 'QA Box serves another model.' }, { ok: false }, 'throw', READY, { ok: false, code: '', error: '' }] }, steps: [P(), P(), P(), P(), C()] },
  machine_abort_in_ready: { engine: [ME()], machines: { ready: ['abort'] }, steps: [P({ abort: 'armed' })] },
  machine_none: { engine: [ME()], steps: [P()] },
  machine_moved: { engine: [ME(), E(), ME({ machine: { ...MT, model: 'qa-other-remote' } })], machines: { ready: [READY] }, fetch: [], steps: [P(), C(), C()] },
  local_to_machine: { engine: [E(), ME()], machines: { ready: [] }, fetch: [list(MODEL)], steps: [P(), C()] },
  machine_connect: { engine: [ME()], machines: { ready: [READY, READY, READY], connectError: true }, machineFetch: [{ throw: 'connect' }, { status: 502, chunks: [] }], steps: [P(), C(), C()] },
  machine_connect_plain: { engine: [ME()], machines: { ready: [READY, READY] }, machineFetch: [{ throw: 'connect' }], steps: [P(), C()] },
  machine_constrained_fallback: { engine: [ME({ provider: YES })], machines: { ready: [READY, READY], connectError: true }, machineFetch: [{ status: 422, chunks: [] }, BAD, okStream()], steps: [P(), C({ constrain: true })] },
};

async function run(sc) {
  const rec = { requests: [], calls: [], log: [], results: [] };
  let clock = 1000;
  const now = () => (clock += 5);
  const seq = (list) => { let i = 0; return () => (list && list.length ? list[Math.min(i++, list.length - 1)] : undefined); };
  const engines = seq(sc.engine || [E()]);
  const budgets = sc.budget ? seq(sc.budget) : null;
  const admissions = sc.admission ? seq(sc.admission) : null;
  const loadeds = sc.loadedModels ? seq(sc.loadedModels) : null;
  const readies = sc.machines ? seq(sc.machines.ready) : null;
  let current = null;
  const queue = (list, transport) => {
    const q = [...(list || [])];
    return async (url, init) => {
      const r = { transport, url, method: init.method, headers: { ...init.headers }, body: init.body === undefined ? null : init.body, reads: 0, cancelled: false };
      rec.requests.push(r);
      const next = q.shift();
      if (!next) throw Error('unscripted request');
      if (next.throw) throw new TypeError('fetch failed');
      if (next.abortThenThrow) { current?.abort(); throw Object.assign(Error('This operation was aborted'), { name: 'AbortError' }); }
      const ok = next.status >= 200 && next.status < 300;
      if (next.chunks) return { ok, status: next.status, body: stubBody(next.chunks, r, () => current?.abort()) };
      return { ok, status: next.status, text: async () => ('text' in next ? next.text : JSON.stringify(next.json)) };
    };
  };
  const deps = {
    engine: () => { rec.calls.push('engine'); const e = engines(); if (e === 'throw') throw Error('QA engine config'); return e; },
    fetch: queue(sc.fetch, 'engine'),
    keep: () => { rec.calls.push('keep'); return sc.keep === 'notArray' ? 'x' : sc.keep || ['qa-rerank']; },
    log: (e) => rec.log.push(enc(e)),
    now,
    roleDefaults: sc.roleDefaults || {},
  };
  if (loadeds) deps.loadedModels = async () => { rec.calls.push('loadedModels'); const v = loadeds(); return v === 'notArray' ? { ids: [] } : v; };
  if (budgets) deps.budgetRefusal = async (model) => { rec.calls.push(`budget:${model}`); const v = budgets(); if (v === 'throw') throw Error('x'); return v; };
  if (admissions) deps.admission = async (work, signal) => {
    rec.calls.push(`admit:${signal ? 'signal' : 'none'}`);
    const v = admissions();
    if (v === 'throw429') throw Object.assign(Error('busy'), { status: 429 });
    if (v === 'throw') throw Error('QA admission');
    if (v === 'abort') current?.abort();
    return work();
  };
  if (sc.machines) deps.machines = {
    ready: async (t) => { rec.calls.push(`ready:${t.baseUrl}`); const v = readies(); if (v === 'throw' || v === undefined) throw Error('x'); if (v === 'abort') { current?.abort(); return READY; } return v; },
    fetch: queue(sc.machineFetch, 'machine'),
    ...(sc.machines.connectError ? { connectError: (t, err) => { rec.calls.push('connectError'); return `QA ${t.machine.name} is unreachable.`; } } : {}),
  };
  const engine = RE.createRoleEngine(deps);
  let session = null;
  for (const step of sc.steps) {
    const ctl = new AbortController();
    current = ctl;
    if (step.abort === 'before') ctl.abort();
    const signal = step.abort ? ctl.signal : null;
    if (step.op === 'pin') {
      const { op, abort, ...args } = step;
      const r = await engine.pinModel({ ...args, signal });
      if (r.ok) session = r.session;
      rec.results.push(enc(r.ok ? { ok: true, session: { taskId: r.session.taskId, model: r.session.model, machine: r.session.machine, thinking: r.session.thinking, roles: [...r.session.roles] } } : r));
    } else if (step.op === 'call') {
      const { op, abort, ...args } = step;
      if (!session) throw Error(`call before a pinned session: ${JSON.stringify(rec.results)}`);
      rec.results.push(enc(await session.call({ ...args, signal })));
    } else if (step.op === 'timings') {
      rec.results.push(enc(session.timings()));
    }
  }
  current = null;
  return { ...sc, requests: rec.requests, calls: rec.calls, log: rec.log, results: rec.results };
}

(async () => {
  const keepAlive = setInterval(() => {}, 1000);
  await Promise.all(sseJobs);
  const scenarios = {};
  for (const [name, sc] of Object.entries(SCENARIOS)) { process.stderr.write(`scenario ${name}\n`); scenarios[name] = enc(await run(sc)); }
  const sha = (n) => crypto.createHash('sha256').update(fs.readFileSync(path.join(dir, n))).digest('hex');
  const sources = {};
  for (const n of ['role-engine.cjs', 'plan-constrained-decoding.cjs', 'stream-guard.cjs', 'sampling-recommendation.cjs', 'model-system.cjs', 'role-context.cjs']) sources[n] = sha(n);
  clearInterval(keepAlive);
  process.stdout.write(JSON.stringify({ generator: 'crates/role-engine/tools/gen-fixtures.cjs', sources, pure, scenarios }, null, 1) + '\n', () => process.exit(0));
})();
