#!/usr/bin/env node
'use strict';
// Recorded model-setting writes for crates/model-writes (track R11): the contract src/budget.rs and src/idle.rs are
// replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/model-writes/tests/record_model_writes.cjs > crates/model-writes/tests/fixtures/node-model-writes.v1.json
//   ... node crates/model-writes/tests/record_model_writes.cjs --check crates/model-writes/tests/fixtures/node-model-writes.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Nothing from node_modules and no dav-parse.wasm. Every expectation comes from noevia-core's own code; the inputs are
// synthetic and QA-named (hand-made edges plus a seeded PRNG), passed through JSON first so Node reads exactly what is
// recorded (a field left out of a case is `undefined`):
//   meminfo  inference-budget.cjs hostRamGib(text) (`throws`: the read failed)
//   budget   PUT /api/models/inference-budget: routes/models.cjs's own lines (the write prelude, then the budget
//            block, cut out of the pinned file and run as they are) with http.cjs readJson over the raw body and a fresh
//            inference-budget.cjs createInferenceBudget over a stub settings store, environment and /proc/meminfo.
//            Recorded: the status, the Cache-Control header, the body, the settings row written, the audit records.
//   idle     PUT /api/models/idle-unload: the same prelude and the idle block over idle-unload-setting.cjs
//            createIdleUnloadSetting with a stub fetchJson (the model manager's GET and PUT answers, or a throw),
//            writePresets (null, a refused or failed hold, or Node's own sequence: write, reloadIf, the scripted reload),
//            reloadPresets and presetsInRouter. Recorded: the status, the header, the body, the manager calls
//            (method, the token header, the body) and the audit records.
//   global   idle-unload-setting.cjs globalIdleSeconds(text)
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
const SOURCE_FILES = ['server/routes/models.cjs', 'server/inference-budget.cjs', 'server/idle-unload-setting.cjs', 'server/http.cjs'];
const sources = Object.fromEntries(SOURCE_FILES.map((f) => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex')]));
const req = (f) => require(path.join(server, f));
const { createInferenceBudget, hostRamGib } = req('inference-budget.cjs');
const { createIdleUnloadSetting, globalIdleSeconds } = req('idle-unload-setting.cjs');
const { readJson } = req('http.cjs');

// ── The lines of routes/models.cjs that are run as they are ─────────────────────────────────
const blocks = {};
const modelsText = fs.readFileSync(path.join(server, 'routes/models.cjs'), 'utf8');
function cut(name, start, end) {
  const from = modelsText.indexOf(start);
  if (from < 0 || modelsText.indexOf(start, from + 1) >= 0) throw new Error(`models.cjs holds ${JSON.stringify(start.slice(0, 60))} ${from < 0 ? 'nowhere' : 'twice'}`);
  const stop = modelsText.indexOf(end, from);
  if (stop < 0) throw new Error(`models.cjs: no end after ${name}`);
  const to = stop + end.length;
  const text = modelsText.slice(from, to);
  blocks[name] = { file: 'server/routes/models.cjs', lines: [modelsText.slice(0, from).split('\n').length, modelsText.slice(0, to).split('\n').length - 1],
    sha256: crypto.createHash('sha256').update(text).digest('hex') };
  return text;
}
const PRELUDE = cut('prelude', "    if (!['GET', 'HEAD'].includes(req.method || 'GET') && /^\\/api\\/(models|model-manager|auto-roles)(\\/|$)/.test(p)) {",
  "      return json(res, 403, { error: 'administrator required' });\n    }\n");
const BUDGET = cut('budget', "    if (p === '/api/models/inference-budget') {", '\n    }\n');
const IDLE = cut('idle', "    if (p === '/api/models/idle-unload') {", '\n    }\n');
const AsyncFunction = (async () => {}).constructor;
const route = new AsyncFunction('req', 'res', 'p', 'authn', 'json', 'readJson', 'inferenceBudget', 'idleUnload', 'service',
  `${PRELUDE}${BUDGET}${IDLE}return 'unhandled';`);

// ── Deterministic inputs ────────────────────────────────────────────────────────────────────
let seed = 0x2026_1115;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const pick = (list) => list[Math.floor(rand() * list.length)];
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));
const clean = (o) => Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined));
const meminfoText = (kib) => `MemTotal:       ${kib} kB\nMemFree:         1 kB\nMemAvailable:   1 kB\n`;

async function run(method, p, role, rawBody, deps) {
  const out = { headers: {} };
  const res = { setHeader: (k, v) => { out.headers[k] = v; } };
  const chunks = rawBody === undefined ? [] : [Buffer.from(rawBody, 'utf8')];
  const reqStub = { method, async *[Symbol.asyncIterator]() { for (const c of chunks) yield c; } };
  const jsonStub = (r, status, body) => { out.status = status; out.body = viaJson(body); };
  const service = { invalidateModelReads: () => { out.invalidated = (out.invalidated || 0) + 1; } };
  const done = await route(reqStub, res, p, { user: { id: 'qa-admin', role } }, jsonStub, readJson, deps.budget ?? null, deps.idle ?? null, service);
  if (done === 'unhandled') out.unhandled = true;
  return out;
}

// ── hostRamGib ──────────────────────────────────────────────────────────────────────────────
const MEMINFO = [null, '', 'garbage', meminfoText(0), meminfoText(1), meminfoText(31142092), meminfoText(65536000), meminfoText(4194304),
  meminfoText(6291456), meminfoText(8388607), meminfoText(1048576 * 5.95), meminfoText(1048576 * 4.05), 'MemTotal:\n  31142092 kB\n',
  'MemTotal: 123', 'xMemTotal: 31142092 kB\n', 'MemFree: 1 kB\nMemTotal:\t31142092kB\n', 'MemTotal: 99999999999999999999 kB\n',
  'MemTotal: 31142092 KB\n', 'MemTotal: 0x10 kB\n', 'MemTotal: -5 kB\nMemTotal: 31142092 kB\n', ' MemTotal: 31142092 kB\n'];
const meminfo = MEMINFO.map((text) => {
  if (text === null) return { throws: true, out: hostRamGib(() => { throw Error('ENOENT'); }) };
  return { text, out: hostRamGib(() => text) };
});

// ── PUT /api/models/inference-budget ────────────────────────────────────────────────────────
const BUDGET_ENVS = [{}, { INFERENCE_MEMORY_BUDGET_GIB: '12' }, { INFERENCE_MEMORY_BUDGET_GIB: 'lots' }, { INFERENCE_MEMORY_BUDGET_GIB: '0' },
  { INFERENCE_MEMORY_BUDGET_GIB: ' 7.5 ' }, { INFERENCE_MEMORY_BUDGET_GIB: '-3' }, { INFERENCE_MEMORY_BUDGET_GIB: '0x10' }];
const ROWS = [undefined, '{"budgetGib":24}', '{"budgetGib":0}', '{"budgetGib":"12"}', 'true', '{not json', '{"budgetGib":100}', 'null', '{}'];
const MEMS = [meminfoText(31142092), meminfoText(65536000), meminfoText(4194304), null, 'garbage', meminfoText(134217728)];
const NUMBERS = [2, 1.9, 25, 25.5, 26, 0, -3, 8.25, 8.3, 0.1, 12, 24.5, 59, 60, 61, 1024, 1025, 2.05, 16.1, 1e-7, 3.0000000000000004, 1e308, 120.9, 124];
const BODIES = [...NUMBERS.map((n) => JSON.stringify({ budgetGib: n })), '{"budgetGib":"16"}', '{"budgetGib":null}', '{}', 'null', '[]', '"x"', '5',
  '{"budgetGib":16,"extra":true}', '{"budgetGib":true}', '{"budgetGib":[16]}', '', '{not json', '{"budgetGib":1e400}', '\ufeff{"budgetGib":12}', '{"budgetGib":12}\u0000', ' {"budgetGib":12} ', '{"budgetGib":12,"budgetGib":13}', '{"budgetGib":-0}',
  `{"pad":"${'x'.repeat(1024 * 1024)}","budgetGib":16}`];
const budget = [];
async function addBudget(c) {
  const rows = new Map();
  if (c.row !== undefined) rows.set('inference:memory-budget', c.row);
  const store = { get: (k) => rows.get(k), set: (k, v) => rows.set(k, v) };
  const audit = [];
  const logs = [];
  const b = c.missing ? null : createInferenceBudget({ store, env: viaJson(c.env), readMeminfo: () => { if (c.mem === null) throw Error('ENOENT'); return c.mem; },
    audit: (...a) => audit.push(viaJson(a)), log: (m) => logs.push(m) });
  const out = await run(c.method, '/api/models/inference-budget', c.role, c.body, { budget: b });
  // The pad case is too large to keep: its length and a hash stand for it.
  const request = c.body !== undefined && c.body.length > 4096 ? { requestBytes: Buffer.byteLength(c.body), requestSha256: crypto.createHash('sha256').update(c.body).digest('hex') } : { request: c.body };
  budget.push({ ...clean({ env: c.env, row: c.row, mem: c.mem, method: c.method, role: c.role, missing: c.missing }), ...request, ...out,
    rowAfter: rows.get('inference:memory-budget') ?? null, audit, logs: logs.length });
}

// ── PUT /api/models/idle-unload ─────────────────────────────────────────────────────────────
const VIEWS = [{ seconds: 1800, source: 'default', fallbackSeconds: 1800, fallbackSource: 'default', defaultSeconds: 1800, minSeconds: 300, maxSeconds: 86400 },
  { seconds: 600, source: 'setting', fallbackSeconds: 3600, fallbackSource: 'env' }, { seconds: 0, source: 'setting' },
  { seconds: 7200, source: 'env', defaultSeconds: 'x', minSeconds: -1, maxSeconds: 1.5, fallbackSource: 'setting' },
  { seconds: -1, source: 'setting' }, { seconds: 600, source: 'qa' }, { seconds: 1.5, source: 'setting' }, null, 'qa', { source: 'setting' }];
const replyOf = (status, body) => ({ ok: status >= 200 && status < 300, status, body });
const GETS = ['throw', replyOf(200, VIEWS[0]), replyOf(200, VIEWS[1]), replyOf(500, { error: 'qa' }), replyOf(200, null)];
const PUTS = ['throw', replyOf(404, {}), replyOf(405, {}), replyOf(400, { error: 'qa' }), replyOf(503, {}), replyOf(500, {}), replyOf(302, {}),
  replyOf(200, null), replyOf(200, 'qa'), ...VIEWS.slice(0, 7).map((v) => replyOf(200, v)),
  ...['written', 'unchanged', 'failed', 'qa', undefined].map((preset) => replyOf(200, { ...VIEWS[1], preset })),
  replyOf(200, { ...VIEWS[2], preset: 'written' }), replyOf(200, { ...VIEWS[0], preset: 'unchanged' })];
const WRITERS = [null, 'refused', 'failed', 'coverage', 'held'];
const RELOADS = [replyOf(200, { reloaded: true }), replyOf(409, { error: 'A model is loaded. Unload it to apply the new settings.', loaded: ['qa-model', 5, 'qa-two'] }),
  replyOf(409, { error: 'qa', loaded: 'qa' }), replyOf(500, {}), { ok: false }, null];
const ROUTER_TEXTS = [null, 'throw', '[*]\nsleep-idle-seconds = 600\n', '[*]\n--sleep-idle-seconds=1800\n[qa]\nmodel=/m\n', '[*]\nsleep-idle-seconds = 0x10\n',
  '[qa]\nsleep-idle-seconds = 600\n', '', 'version = 1\r\n[*]\r\n  -sleep-idle-seconds  =  7200  \r\n', '[*]\nsleep-idle-seconds\n[*]\nsleep-idle-seconds = 3600\n',
  '[*]\nsleep-idle-seconds = 600\nsleep-idle-seconds = 900\n'];
const SECONDS_BODIES = ['{"seconds":600}', '{"seconds":0}', '{"seconds":null}', '{"seconds":300}', '{"seconds":299}', '{"seconds":86400}', '{"seconds":86401}',
  '{"seconds":600.5}', '{"seconds":"600"}', '{}', '[]', 'null', '"x"', '{"seconds":1e3}', '{"seconds":-0}', '', '{not json', '{"seconds":1800,"other":1}', '7'];
const idle = [];
async function addIdle(c) {
  const calls = [];
  const audit = [];
  const env = viaJson(c.env);
  const fetchJson = async (url, init, timeout) => {
    calls.push(clean({ url, method: init.method, token: init.headers['X-Model-Loader-Token'], contentType: init.headers['Content-Type'], body: init.body, timeout }));
    const scripted = init.method === 'GET' ? c.get : c.put;
    if (scripted === 'throw') throw Error('qa: no answer');
    return viaJson(scripted);
  };
  let writePresets = null;
  if (c.writer === 'refused') writePresets = async () => { throw Object.assign(Error('held'), { status: 409, holdRefused: true }); };
  else if (c.writer === 'failed') writePresets = async () => { throw Object.assign(Error('The model service could not confirm that chat is idle, so nothing was changed. Try again shortly.'), { status: 503 }); };
  else if (c.writer === 'coverage') writePresets = async (write, reloadIf) => { const w = await write(); if (!reloadIf(w)) return { written: w, reload: null }; throw Object.assign(Error('The model service stopped confirming that chat is held back, so the reload stopped. Try again shortly.'), { status: 503, coverageLost: true }); };
  else if (c.writer === 'held') writePresets = async (write, reloadIf) => { const w = await write(); if (!reloadIf(w)) return { written: w, reload: null }; return { written: w, reload: viaJson(c.reload) ?? { ok: false } }; };
  const setting = c.missing ? null : createIdleUnloadSetting({ env, fetchJson,
    reloadPresets: c.reloader ? async () => { calls.push({ reload: true }); return viaJson(c.reload); } : null,
    writePresets,
    presetsInRouter: async () => { calls.push({ routerText: true }); if (c.routerText === 'throw') throw Error('qa'); return c.routerText; },
    audit: (...a) => audit.push(viaJson(a)) });
  const out = await run(c.method, '/api/models/idle-unload', c.role, c.body, { idle: setting });
  idle.push({ ...clean({ env: c.env, method: c.method, role: c.role, missing: c.missing, request: c.body, get: c.get, put: c.put, writer: c.writer,
    reloader: c.reloader, reload: c.reload, routerText: c.routerText }), ...out, calls, audit });
}

(async () => {
  for (const method of ['PUT', 'POST', 'DELETE', 'PATCH']) for (const role of ['admin', 'member']) {
    await addBudget({ env: {}, mem: MEMS[0], method, role, body: '{"budgetGib":12}' });
    await addIdle({ env: { MODEL_LOADER_URL: 'http://qa-loader.invalid' }, method, role, body: '{"seconds":600}', get: GETS[1], put: PUTS[16], writer: 'held', reload: RELOADS[0], routerText: ROUTER_TEXTS[2] });
  }
  await addBudget({ env: {}, mem: MEMS[0], method: 'PUT', role: 'admin', body: '{"budgetGib":12}', missing: true });
  for (const body of BODIES) await addBudget({ env: {}, mem: MEMS[0], method: 'PUT', role: 'admin', body });
  for (let n = 0; n < 300; n += 1) {
    await addBudget({ env: pick(BUDGET_ENVS), row: pick(ROWS), mem: pick(MEMS), method: 'PUT', role: 'admin', body: rand() < 0.7 ? JSON.stringify({ budgetGib: pick(NUMBERS) }) : pick(BODIES.slice(0, -1)) });
  }

  await addIdle({ env: {}, method: 'PUT', role: 'admin', body: '{"seconds":600}', missing: true });
  for (const body of SECONDS_BODIES) {
    await addIdle({ env: { MODEL_LOADER_URL: 'http://qa-loader.invalid/' }, method: 'PUT', role: 'admin', body, get: GETS[1], put: replyOf(200, { ...VIEWS[1], preset: 'written' }), writer: 'held', reload: RELOADS[0], routerText: ROUTER_TEXTS[2] });
  }
  for (let n = 0; n < 500; n += 1) {
    const writer = pick(WRITERS);
    await addIdle({ env: pick([{ MODEL_LOADER_URL: 'http://qa-loader.invalid' }, { MODEL_LOADER_URL: 'http://qa-loader.invalid//', MODEL_LOADER_TOKEN: 'qa-loader-token' }, { MODEL_LOADER_URL: '' }]),
      method: 'PUT', role: 'admin', body: rand() < 0.8 ? pick(SECONDS_BODIES.slice(0, 7)) : pick(SECONDS_BODIES), get: pick(GETS), put: pick(PUTS),
      writer, reloader: writer === null ? rand() < 0.7 : undefined, reload: pick(RELOADS), routerText: pick(ROUTER_TEXTS) });
  }
  const global = ROUTER_TEXTS.filter((t) => typeof t === 'string' && t !== 'throw').concat(['[*]\nsleep-idle-seconds=12', '[*]\n sleep-idle-seconds = 12 \n',
    '[*]\r\nsleep-idle-seconds=5\r\n', '[*]\nother=1\n', '[*] \nsleep-idle-seconds=5', '[ * ]\nsleep-idle-seconds=5', '[*]\n---sleep-idle-seconds=5',
    '[*]\nsleep-idle-seconds=-5', '[*]\nsleep-idle-seconds= 1 2', '[*]\n=5\nsleep-idle-seconds=6', '[*]\nsleep-idle-seconds=99999999999999999999'])
    .map((text) => ({ text, out: globalIdleSeconds(text) }));

  // ── Output ────────────────────────────────────────────────────────────────────────────────
  const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
  const text = `{"version":1,"generator":"crates/model-writes/tests/record_model_writes.cjs",\n"node":${JSON.stringify(process.version)},\n`
    + `"sources":${JSON.stringify(sources)},\n"blocks":${JSON.stringify(blocks)},\n`
    + `"meminfo":[\n${lines(meminfo)}\n],\n"budget":[\n${lines(budget)}\n],\n"idle":[\n${lines(idle)}\n],\n"global":[\n${lines(global)}\n],\n"differences":[]}\n`;
  const counts = `${meminfo.length} meminfo, ${budget.length} budget (${budget.filter((b) => b.status === 200).length} saved), `
    + `${idle.length} idle (${idle.filter((b) => b.status === 200).length} saved), ${global.length} global`;
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
