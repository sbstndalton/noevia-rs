#!/usr/bin/env node
'use strict';
// Recorded /api/model-manager/* proxy decisions for crates/model-manager-proxy (track R11): the contract src/ is
// replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/model-manager-proxy/tests/record_model_manager_proxy.cjs > crates/model-manager-proxy/tests/fixtures/node-model-manager-proxy.v1.json
//   ... node crates/model-manager-proxy/tests/record_model_manager_proxy.cjs --check crates/model-manager-proxy/tests/fixtures/node-model-manager-proxy.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Nothing from node_modules and no dav-parse.wasm. Every expectation comes from noevia-core's own code: the real
// routes/models.cjs createModelRoutes(...) (its write prelude, the model-manager block and the wrapper around handle)
// with chat-model-kind.cjs, benchmark-sweep-guard.cjs createSweepGuard, benchmark-evidence.cjs, model-system.cjs
// isSystemModel (over a synthetic models.roles.json) and http.cjs readBody / errorResponse, run as they are. Stubbed is
// only the outside world, and every stub logs what it was asked, in order:
//   fetchJson (the model manager), modelManager.loadRefusal / presetRefusal / sizingBudgetGib / holdMaintenance (the
//   sweep guard's hold and its release) / writePresets / reloadPresets / recordEvidence / unload, service.modelScanCache
//   / servedCatalogue / clearRoleReferences / forgetModelState / invalidateModelReads, model-system.cjs
//   isSidecarModel and rerankModelNames (the embedding service and reranker settings), embedding-service.cjs current()
//   (protectionKnown) and rerank-settings.cjs's lock (withSettings). The sweep guard's watch is wrapped to log the
//   models it was given; its timers never fire.
// A thrown request is answered as index.cjs answers it: http.cjs errorResponse(err).
// The inputs are synthetic and QA-named (hand-made edges plus a seeded PRNG), passed through JSON first so Node reads
// exactly what is recorded. `evidence` calls benchmark-evidence.cjs throughputRecords(detail, { now }) directly.
// Each source file's sha256 is recorded, so a changed Node file needs a fresh recording. Node's console output goes to
// stderr; stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
const server = path.join(root, 'server');
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => process.stderr.write(`${a.map(String).join(' ')}\n`);
const SOURCE_FILES = ['server/routes/models.cjs', 'server/model-system.cjs', 'server/chat-model-kind.cjs', 'server/benchmark-sweep-guard.cjs',
  'server/benchmark-evidence.cjs', 'server/http.cjs', 'server/embedding-service.cjs', 'server/rerank-settings.cjs'];
const sources = Object.fromEntries(SOURCE_FILES.map((f) => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex')]));
const req = (f) => require(path.join(server, f));

// ── The fixed clock (throughputRecords' Date.now()) ─────────────────────────────────────────
let NOW = 0;
Date.now = () => NOW;

// ── Stubs of the outside world, installed before routes/models.cjs reads them ──────────────
let log = null;   // the current case's calls
let sc = null;    // the current case's script
const thrownOf = (t) => Object.assign(Error(t.message), t.status === undefined ? {} : { status: t.status });
const system = req('model-system.cjs');
const realIsSystemModel = system.isSystemModel;
system.isSystemModel = function isSystemModel(id, modelPath) {
  const key = id === undefined || id === null ? '' : String(id).trim();
  const answer = realIsSystemModel(id, modelPath);
  if (key) log.push({ call: 'isSystemModel', id: key, answer });
  return answer;
};
system.isSidecarModel = function isSidecarModel(name) {
  const n = String(name || '').trim();
  if (!n) return false;
  if (sc.sidecarThrows) { log.push({ call: 'isSidecarModel', name: n, threw: sc.sidecarThrows }); throw thrownOf(sc.sidecarThrows); }
  const answer = (sc.sidecars || []).includes(n);
  log.push({ call: 'isSidecarModel', name: n, answer });
  return answer;
};
// rerankModelNames(env) throws (503) or answers; the route only asks .has(n), so the throw moves to has().
system.rerankModelNames = function rerankModelNames() {
  return { has(n) {
    if (sc.rerankThrows) { log.push({ call: 'isRerankerModel', name: n, threw: sc.rerankThrows }); throw thrownOf(sc.rerankThrows); }
    const answer = (sc.rerankers || []).includes(n);
    log.push({ call: 'isRerankerModel', name: n, answer });
    return answer;
  } };
};
const embedding = req('embedding-service.cjs');
embedding.current = () => {
  const kind = sc.embedding || 'known';
  const service = kind === 'none' ? null : kind === 'legacy' ? {} : { protectionKnown: () => kind === 'known' };
  log.push({ call: 'embeddingProtectionUnknown', answer: !!service?.protectionKnown && !service.protectionKnown() });
  return service;
};
let locked = false;
req('rerank-settings.cjs').install({ run: (fn) => { locked = true; return fn(); } });
const sweep = req('benchmark-sweep-guard.cjs');
const realCreateSweepGuard = sweep.createSweepGuard;
sweep.createSweepGuard = (options) => {
  const guard = realCreateSweepGuard(options);
  const watch = guard.watch;
  guard.watch = (release, models) => { log.push({ call: 'sweepWatch', models }); return watch(release, models); };
  return guard;
};
// isSystemModel follows a synthetic roles file beside a synthetic models.ini.
const rolesDir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-mm-proxy-'));
fs.writeFileSync(path.join(rolesDir, 'models.ini'), '');
fs.writeFileSync(path.join(rolesDir, 'models.roles.json'), JSON.stringify({ 'qa-router': 'system-routing', 'qa-old-router': 'laya', 'laya-qa-chat': 'chat', 'qa-sys ✓': 'System-Routing' }));
system.useRolesBeside(path.join(rolesDir, 'models.ini'));

const { createModelRoutes } = req('routes/models.cjs');
const { readBody, readJson, errorResponse } = req('http.cjs');
const { throughputRecords } = req('benchmark-evidence.cjs');

// ── Deterministic inputs ────────────────────────────────────────────────────────────────────
let seed = 0x2026_1114;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const pick = (list) => list[Math.floor(rand() * list.length)];
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));
const clean = (o) => Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined));

// A body of exactly `total` bytes: `{"aliases":["qa-chat"],"pad":"xxx…"}`.
function padded(total) {
  const head = '{"aliases":["qa-chat"],"pad":"', tail = '"}';
  return head + 'x'.repeat(total - head.length - tail.length) + tail;
}
const bytesOf = (c) => (c.base64 !== undefined ? Buffer.from(c.base64, 'base64') : c.padded !== undefined ? Buffer.from(padded(c.padded), 'utf8') : c.body === undefined ? undefined : Buffer.from(c.body, 'utf8'));

const ALL_CAPS = ['loadRefusal', 'presetRefusal', 'sizingBudgetGib', 'writePresets', 'reloadPresets', 'recordEvidence', 'unload', 'holdMaintenance'];
const DEFAULT_ENV = { url: 'http://qa-loader.invalid:8090/', token: 'qa-loader-token' };
const OK = { ok: true, status: 200, body: { ok: true } };

// ── One request through the real routes ─────────────────────────────────────────────────────
async function runCase(c) {
  const calls = [];
  log = calls;
  sc = c;
  NOW = c.now ?? 1_800_000_000_000;
  locked = false;
  const caps = new Set(c.caps ?? ALL_CAPS);
  const fetches = [...(c.fetch ?? [OK])];
  const unloads = [...(c.unload ?? [])];
  const roleRefs = [...(c.roleRefs ?? [])];
  const forgets = [...(c.forget ?? [])];
  let scriptError = null;
  let constructing = true;
  const env = {};
  const e = c.env === undefined ? DEFAULT_ENV : c.env;
  if (e.url !== undefined) env.MODEL_LOADER_URL = e.url;
  if (e.token !== undefined) env.MODEL_LOADER_TOKEN = e.token;
  const fetchJson = async (url, init, timeoutMs) => {
    if (constructing) return { ok: true, status: 200, body: { job: null } }; // the sweep guard's adopt() at start
    const entry = { call: 'fetch', url, method: init.method, headers: init.headers, ...(init.body === undefined ? {} : { body: init.body }), timeoutMs };
    log.push(entry);
    if (!fetches.length) { scriptError = `${c.name}: no scripted answer for ${url}`; entry.answer = null; throw Error('qa: no answer'); }
    const a = viaJson(fetches.shift());
    if (a === 'throw') { entry.answer = null; throw Error('qa: connection refused'); }
    if (a.overflow !== undefined) { entry.answer = { ok: false, status: a.overflow }; return { ok: false, status: a.overflow, error: 'response too large' }; }
    entry.answer = a;
    return viaJson(a);
  };
  let cacheEntry = c.scan === undefined ? undefined : { at: 1, body: viaJson(c.scan) };
  const service = {
    modelScanCache: {
      get: (k) => { calls.push({ call: 'scanCacheGet', key: k, ...(cacheEntry === undefined ? {} : { answer: viaJson(cacheEntry.body) }) }); return cacheEntry === undefined ? undefined : { at: cacheEntry.at, body: viaJson(cacheEntry.body) }; },
      set: (k, v) => { calls.push({ call: 'scanCacheSet', key: k, body: viaJson(v.body) }); cacheEntry = { at: v.at, body: viaJson(v.body) }; },
      clear: () => { calls.push({ call: 'scanCacheClear' }); cacheEntry = undefined; },
    },
    servedCatalogue: async () => {
      if (c.catalogue && c.catalogue.throw) { calls.push({ call: 'servedCatalogue', threw: c.catalogue.throw }); throw thrownOf(c.catalogue.throw); }
      calls.push({ call: 'servedCatalogue', ...(c.catalogue === undefined ? {} : { answer: c.catalogue }) });
      return viaJson(c.catalogue);
    },
    clearRoleReferences: async (id) => {
      const a = roleRefs.length ? roleRefs.shift() : [];
      if (a && a.throw) { calls.push({ call: 'clearRoleReferences', id, threw: a.throw }); throw thrownOf(a.throw); }
      calls.push({ call: 'clearRoleReferences', id, answer: a });
      return viaJson(a);
    },
    forgetModelState: async (id) => {
      const a = forgets.length ? forgets.shift() : 'ok';
      if (a && a.throw) { calls.push({ call: 'forgetModelState', id, threw: a.throw }); throw thrownOf(a.throw); }
      calls.push({ call: 'forgetModelState', id });
    },
    invalidateModelReads: () => calls.push({ call: 'invalidateModelReads' }),
    modelsInstalled: async () => [],
    autoRoles: () => null,
    deriveUserModelName: (x) => x,
  };
  const mm = {};
  if (caps.has('loadRefusal')) mm.loadRefusal = async (alias) => {
    const a = (c.loadRefusals || {})[alias];
    if (a === 'reject') { calls.push({ call: 'loadRefusal', alias, answer: null }); throw Error('qa: the budget check failed'); }
    calls.push({ call: 'loadRefusal', alias, answer: a ?? null });
    return viaJson(a ?? null);
  };
  if (caps.has('presetRefusal')) mm.presetRefusal = async (name, options) => {
    const a = c.presetRefusal ?? null;
    if (a && a.throw) { calls.push({ call: 'presetRefusal', name, options: viaJson(options), threw: a.throw }); throw thrownOf(a.throw); }
    calls.push({ call: 'presetRefusal', name, options: viaJson(options), answer: a });
    return viaJson(a);
  };
  if (caps.has('sizingBudgetGib')) mm.sizingBudgetGib = () => { calls.push({ call: 'sizingBudgetGib', ...(c.budget === undefined ? {} : { answer: c.budget }) }); return viaJson(c.budget); };
  if (caps.has('writePresets')) mm.writePresets = async (write, reloadIf) => {
    const w = c.writer ?? 'grant';
    if (w === 'refuse') { calls.push({ call: 'presetHold', answer: 'refused' }); throw Object.assign(Error('held'), { status: 409, holdRefused: true }); }
    if (w.throwBefore) { calls.push({ call: 'presetHold', threw: w.throwBefore }); throw thrownOf(w.throwBefore); }
    calls.push({ call: 'presetHold', answer: 'granted' });
    try {
      const written = await write();
      if (!reloadIf(written)) return { written, reload: null };
      if (w.reloadThrows) { calls.push({ call: 'presetReload', threw: w.reloadThrows }); throw thrownOf(w.reloadThrows); }
      calls.push({ call: 'presetReload' });
      return { written, reload: { ok: true } };
    } finally { calls.push({ call: 'presetRelease' }); }
  };
  if (caps.has('reloadPresets')) mm.reloadPresets = async () => { calls.push({ call: 'reloadPresets' }); if (c.reloadRejects) throw Error('qa: reload failed'); return { ok: true }; };
  if (caps.has('recordEvidence')) mm.recordEvidence = (model, record) => { calls.push({ call: 'recordEvidence', ...(model === undefined ? {} : { model }), record: viaJson(record) }); return Promise.resolve(); };
  if (caps.has('unload')) mm.unload = async (id) => {
    const a = unloads.length ? unloads.shift() : { ok: true };
    if (a === 'reject') { calls.push({ call: 'unload', id, answer: false }); throw Error('qa: unload failed'); }
    calls.push({ call: 'unload', id, answer: !!a?.ok });
    return viaJson(a);
  };
  if (caps.has('holdMaintenance')) mm.holdMaintenance = (reason, options) => {
    const h = c.sweepHold ?? 'grant';
    const entry = { call: 'sweepHold', reason, recovery: options?.recovery };
    calls.push(entry);
    const release = () => calls.push({ call: 'sweepRelease' });
    if (h === 'grant') { entry.answer = 'granted'; return release; }
    if (h === 'grantLater') { entry.answer = 'granted'; return Promise.resolve(release); }
    if (h.throw) { entry.refused = h.throw; throw Object.assign(Error('qa: hold refused'), h.throw); }
    entry.refused = h.reject;
    return Promise.reject(Object.assign(Error('qa: hold refused'), h.reject));
  };
  const out = { headers: {} };
  const routes = createModelRoutes({
    json: (_res, status, body) => { out.status = status; out.bodyJson = JSON.stringify(body); },
    readBody, readJson, fetchJson, env, modelManager: mm, currentWorkspace: () => ({ userId: 'qa-admin' }), service,
    sweepGuardOptions: { setTimer: () => null, clearTimer: () => {}, log: () => {} },
  });
  await routes.sweepAdopted;
  constructing = false;
  calls.length = 0;
  locked = false;

  let p, search, url;
  if (c.target !== undefined) {
    url = new URL(c.target, 'http://localhost');
    p = url.pathname; search = url.search;
  } else {
    p = c.path; search = c.search ?? '';
    url = { search };
  }
  const raw = bytesOf(c);
  const chunks = raw === undefined ? [] : [raw];
  const reqStub = { method: c.method, headers: {}, url: p + search, async *[Symbol.asyncIterator]() { for (const x of chunks) yield x; } };
  const res = { setHeader: (k, v) => { out.headers[k] = v; } };
  let threw = false;
  try {
    const handled = await routes(reqStub, res, { path: p, authn: { user: { id: 'qa-admin', role: c.role ?? 'admin' } }, url });
    if (!handled) out.passed = true;
  } catch (err) {
    threw = true;
    const failure = errorResponse(err);
    out.status = failure.status; out.bodyJson = JSON.stringify(failure.body);
  }
  // Fire-and-forget evidence records land on the next turns.
  await new Promise((r) => setImmediate(r));
  routes.cancelSweepAdopt();
  if (scriptError) throw Error(scriptError);
  return { p, search, expect: clean({ status: out.status, bodyJson: out.bodyJson, noStore: out.headers['Cache-Control'] === 'no-store', threw, locked, passed: out.passed, calls }) };
}

// ── Cases ───────────────────────────────────────────────────────────────────────────────────
const S = [];
const add = (name, c) => S.push({ name, method: 'GET', ...c });
const MM = '/api/model-manager/';
const T = (rest) => MM + rest;

// Access and configuration.
add('member GET is refused', { target: T('models'), role: 'member' });
add('member POST is refused (reads dropped, no lock)', { method: 'POST', target: T('benchmark/start'), role: 'member', body: '{}' });
add('member DELETE section is refused', { method: 'DELETE', target: T('sections/qa'), role: 'member' });
add('owner role is not admin', { target: T('models'), role: 'owner' });
add('no model loader URL', { target: T('models'), env: {} });
add('empty model loader URL', { method: 'POST', target: T('benchmark/start'), env: { url: '' }, body: '{}' });
add('no token: no token header', { target: T('benchmark/runs'), env: { url: 'http://qa-loader.invalid' } });
add('empty token: no token header', { target: T('benchmark/runs'), env: { url: 'http://qa-loader.invalid', token: '' } });
add('loader URL with many trailing slashes', { target: T('status'), env: { url: 'http://qa-loader.invalid:9///', token: 'qa-loader-token' } });
add('loader URL with a path', { target: T('status'), env: { url: 'http://qa-loader.invalid/base/', token: 'qa-loader-token' } });
add('not a model-manager path passes', { target: '/api/model-managerx' });
add('the bare prefix without slash passes', { method: 'POST', target: '/api/model-manager', body: '{}' });

// The raw path.
const PATHS = ['', 'a', 'a/', 'a//b', 'models', 'a b', 'a%20b', 'a..b', '..', 'x/../y', '%2e', '%2E%2E', 'sections/%2e', 'sections/%2E%2e', 'a%2Fb', 'a%2fb',
  'a%5Cb', 'a%5c', '%', 'a%', 'a%2', '%zz', '%C3', '%C3%A9', '%c3%a9', '%E2%9C%93', '%F0%9F%98%80', '%ED%A0%80', '%F4%90%80%80', '%C0%AF', '%E0%80%AF',
  '%F0%80%80%80', '%80', '%BF', '%FE', '%FF', '%C3%28', '%E2%82', '%00', 'a:b', 'a@b', 'a+b', 'a-b_c.d', 'a~b', "a'b", 'a(b)', 'a*b', 'a!b', 'a$b', 'a,b', 'a;b',
  'a=b', 'a[b]', 'a|b', 'a^b', 'a`b', 'é', '✓', 'sections/qa%25', 'sections/%2525', '%25', '.', 'a/.', './a', '.a', 'a.', '...', 'a/..b', 'sections/%2E',
  'benchmark%2Fstart', 'benchmark/st%61rt', 'models%2Fdelete', 'models/d%65lete', 'sections/%71a', 'sections/a%2Fb', 'sections/a%5Cb', 'sections/%E2%9C%93/rename'];
for (const rest of PATHS) {
  add(`raw path ${JSON.stringify(rest)} GET`, { path: T(rest) });
  add(`raw path ${JSON.stringify(rest)} POST`, { method: 'POST', path: T(rest), body: '{}' });
}
for (const target of [T('a b'), T('é/✓'), T('sections/qa model'), T('a"b'), T('a<b>'), T('a{b}'), T('./x'), T('x/./y'), T('x/../models'), T('%2e%2e/x'),
  T('sections/%2e%2E'), T('a#frag'), T('a?'), T('a?x=1&y=%20+z'), T('a?é=✓'), T('a\\b'), T('a\tb'), T('sections/qa?budget_gib=4')]) {
  add(`target ${JSON.stringify(target)}`, { target });
}

// Methods, bodies and the 1 MiB limit.
for (const method of ['GET', 'HEAD', 'DELETE', 'POST', 'PUT', 'PATCH', 'OPTIONS', 'PROPFIND']) {
  add(`${method} with a body`, { method, target: T('qa/thing?x=1'), body: '{"qa":true}' });
  add(`${method} without a body`, { method, target: T('qa/thing') });
}
add('POST body over 1 MiB', { method: 'POST', target: T('qa/thing'), padded: 1024 * 1024 + 1 });
add('POST body of exactly 1 MiB', { method: 'POST', target: T('qa/thing'), padded: 1024 * 1024 });
add('GET body over 1 MiB is not read', { target: T('qa/thing'), padded: 1024 * 1024 + 1 });
add('DELETE body over 1 MiB is not read', { method: 'DELETE', target: T('qa/thing'), padded: 1024 * 1024 + 1 });
add('PUT invalid UTF-8 is forwarded as read', { method: 'PUT', target: T('models-ini'), base64: Buffer.concat([Buffer.from('{"text":"'), Buffer.from([0xff, 0xc3, 0x28, 0xe2, 0x82, 0xf0, 0x9f]), Buffer.from('"}')]).toString('base64') });
add('POST BOM body', { method: 'POST', target: T('qa'), base64: Buffer.from([0xef, 0xbb, 0xbf, 0x7b, 0x7d]).toString('base64') });
add('POST over 1 MiB on benchmark/start', { method: 'POST', target: T('benchmark/start'), padded: 1024 * 1024 + 1 });

// benchmark/start.
const CAT = [{ name: 'qa-chat', labels: ['chat'] }, { name: 'qa-embed-x', labels: [] }, { name: 'qa-vectors', labels: ['Embedding'] }, { name: 'qa-scorer', labels: ['RERANK'] },
  { name: 'qa-plain' }, null, 7, { name: 'qa-odd', labels: 'embedding' }, { name: 'qa-reranker2', labels: ['reranker '] }, { name: 'qa-reranking', labels: [' reranking'] },
  { name: 'qa-num', labels: [5, null, 'embeddings'] }, { name: 'laya-qa-chat', labels: [] }];
const start = (name, aliases, extra = {}) => add(`benchmark/start: ${name}`, { method: 'POST', target: T('benchmark/start'), body: aliases === undefined ? '{}' : JSON.stringify({ aliases }), catalogue: CAT, ...extra });
start('chat models only', ['qa-chat', 'qa-plain']);
start('one embedding by name', ['qa-chat', 'qa-embed-x']);
start('two non-chat', ['qa-embed-x', 'qa-vectors']);
start('three non-chat', ['qa-vectors', 'qa-scorer', 'qa-Rerank-z']);
start('a label with spaces is chat', ['qa-reranker2', 'qa-reranking']);
start('labels that are not an array', ['qa-odd']);
start('labels with non-strings', ['qa-num']);
start('the system router by role', ['qa-router']);
start('the legacy role key', ['qa-old-router']);
start('a laya name with a chat role', ['laya-qa-chat']);
start('a laya name without a role', ['laya', 'Laya-7b', 'laya_x', 'layax', ' laya.q ']);
start('a role with unicode', ['qa-sys ✓', ' qa-sys ✓ ']);
start('non-string aliases', [0, 1, null, true, false, {}, [], ['qa-chat'], [null], '', ' ']);
start('an alias object with toString throws', [{ toString: 'x' }]);
start('aliases not an array', 'qa-embed');
start('no aliases', undefined);
start('a missing catalogue entry by name', ['qa-EMBED-missing', 'qa-unknown']);
start('the catalogue is null', ['qa-chat', 'qa-embed'], { catalogue: null });
start('the catalogue is not an array', ['qa-vectors'], { catalogue: { name: 'qa-vectors', labels: ['embedding'] } });
start('the catalogue read throws', ['qa-chat'], { catalogue: { throw: { message: 'qa catalogue down' } } });
start('the catalogue read throws a 4xx', ['qa-chat'], { catalogue: { throw: { status: 409, message: 'qa catalogue busy' } } });
start('lone surrogates', ['qa-\ud800-embed', 'qa-\udfff']);
start('emoji alias', ['qa-😀-rerank']);
add('benchmark/start: invalid JSON reads as {}', { method: 'POST', target: T('benchmark/start'), body: '{nope', catalogue: CAT });
add('benchmark/start: empty body', { method: 'POST', target: T('benchmark/start'), body: '', catalogue: CAT });
add('benchmark/start: null body throws', { method: 'POST', target: T('benchmark/start'), body: 'null', catalogue: CAT });
add('benchmark/start: array body', { method: 'POST', target: T('benchmark/start'), body: '["qa-embed"]', catalogue: CAT });
add('benchmark/start: number body', { method: 'POST', target: T('benchmark/start'), body: '5', catalogue: CAT });
add('benchmark/start: string body', { method: 'POST', target: T('benchmark/start'), body: '"x"', catalogue: CAT });
add('benchmark/start: number out of range goes to core', { method: 'POST', target: T('benchmark/start'), body: '{"aliases":["qa-embed"],"x":1e999}', catalogue: CAT });
start('load refusal on the second alias', ['qa-chat', 'qa-big', 'qa-bigger'], { loadRefusals: { 'qa-big': { error: 'qa: does not fit', code: 'budget', needGib: 20 } } });
start('load refusal rejected is no refusal', ['qa-chat', 'qa-big'], { loadRefusals: { 'qa-big': 'reject' } });
start('falsy load refusals', ['qa-a', 'qa-b', 'qa-c', 'qa-d'], { loadRefusals: { 'qa-a': 0, 'qa-b': '', 'qa-c': false, 'qa-d': null } });
start('truthy odd load refusals', ['qa-a'], { loadRefusals: { 'qa-a': 'qa no' } });
start('truthy odd load refusals 2', ['qa-a'], { loadRefusals: { 'qa-a': [] } });
start('truthy odd load refusals 3', ['qa-a'], { loadRefusals: { 'qa-a': 7 } });
start('no loadRefusal', ['qa-chat', 'qa-big'], { caps: ALL_CAPS.filter((x) => x !== 'loadRefusal'), loadRefusals: { 'qa-big': { error: 'x' } } });
start('only strings are budget-checked, up to 64', [5, 'qa-0', ...Array.from({ length: 70 }, (_, i) => `qa-${i + 1}`)], { loadRefusals: { 'qa-65': { error: 'qa 65' }, 'qa-64': { error: 'qa 64' } } });
start('the 65th string is never checked', Array.from({ length: 70 }, (_, i) => `qa-${i}`), { loadRefusals: { 'qa-64': { error: 'qa 64' } } });
start('empty strings are budget-checked', ['', 'qa-chat'], { loadRefusals: { '': { error: 'qa empty' } } });
start('forward answers 202', ['qa-chat'], { fetch: [{ ok: true, status: 202, body: { run: 4 } }] });

// benchmark/sweep.
const sweepCase = (name, aliases, extra = {}) => add(`benchmark/sweep: ${name}`, { method: 'POST', target: T('benchmark/sweep'), body: JSON.stringify({ aliases }), ...extra });
sweepCase('two models: hold then watch', ['qa-a', 'qa-b'], { fetch: [{ ok: true, status: 202, body: { job: 1 } }] });
sweepCase('the manager refuses: release', ['qa-a'], { fetch: [{ ok: false, status: 409, body: { detail: 'qa busy' } }] });
sweepCase('no answer: watch', ['qa-a', 'qa-b', 'qa-c'], { fetch: ['throw'] });
sweepCase('a hold granted later', ['qa-a'], { sweepHold: 'grantLater' });
sweepCase('no aliases: no hold', []);
sweepCase('only non-strings and empty: no hold', [0, '', null, {}]);
sweepCase('64 models', Array.from({ length: 64 }, (_, i) => `qa-${i}`));
sweepCase('65 models', Array.from({ length: 65 }, (_, i) => `qa-${i}`));
sweepCase('65 entries but 64 strings', [...Array.from({ length: 64 }, (_, i) => `qa-${i}`), '']);
sweepCase('load refusal', ['qa-a', 'qa-b'], { loadRefusals: { 'qa-b': { error: 'qa: too big' } } });
sweepCase('load refusal rejected', ['qa-a'], { loadRefusals: { 'qa-a': 'reject' } });
sweepCase('no loadRefusal', ['qa-a'], { caps: ALL_CAPS.filter((x) => x !== 'loadRefusal') });
sweepCase('no sweep guard', ['qa-a'], { caps: ALL_CAPS.filter((x) => x !== 'holdMaintenance') });
sweepCase('the lease authority is not answering', ['qa-a'], { sweepHold: { throw: { status: 503, code: 'maintenanceAuthorityUnavailable' } } });
sweepCase('the lease authority rejects later', ['qa-a'], { sweepHold: { reject: { status: 503, code: 'maintenanceAuthorityUnavailable' } } });
sweepCase('another 503 is busy', ['qa-a'], { sweepHold: { throw: { status: 503, code: 'qa_other' } } });
sweepCase('a 503 as text is busy', ['qa-a'], { sweepHold: { throw: { status: '503', code: 'maintenanceAuthorityUnavailable' } } });
sweepCase('the gate refuses', ['qa-a'], { sweepHold: { reject: { status: 409, fromGate: true, code: 'engine_busy' } } });
sweepCase('a chat is running', ['qa-a'], { sweepHold: { throw: { chatActive: true } } });
sweepCase('the gate does not answer', ['qa-a'], { sweepHold: { reject: { status: 502 } } });
sweepCase('a plain failure', ['qa-a'], { sweepHold: { throw: {} } });
sweepCase('the forward overflows: release', ['qa-a'], { fetch: [{ overflow: 200 }] });
add('benchmark/sweep: null body throws', { method: 'POST', target: T('benchmark/sweep'), body: 'null' });
add('benchmark/sweep: invalid JSON', { method: 'POST', target: T('benchmark/sweep'), body: '{"aliases":' });
add('benchmark/sweep: GET is plain', { target: T('benchmark/sweep') });
add('benchmark/sweep: encoded path', { method: 'POST', path: T('benchmark/sw%65ep'), body: '{"aliases":["qa-a"]}' });

// models/delete.
const SCAN = { models: [
  { key: 'k-chat', modelId: 'qa-chat', sections: ['qa-chat', 'qa-chat-long'] },
  { key: 'k-router', modelId: 'qa-router', sections: [] },
  { key: 'k-laya', modelId: 'qa-misc', sections: ['laya-mini'] },
  { key: 'k-embed', modelId: 'qa-embedder', sections: ['qa-embedder'] },
  { key: 'k-rerank', modelId: 'qa-other', sections: ['qa-ranker'] },
  { key: 'k-nosec', modelId: 'qa-solo' },
  { key: 'k-num', modelId: 7, sections: [8, 'qa-chat'] },
  { key: 'k-empty', modelId: '', sections: null },
  { key: 5, modelId: 'qa-five', sections: [] },
] };
const del = (name, models, extra = {}) => add(`models/delete: ${name}`, { method: 'POST', target: T('models/delete'), body: JSON.stringify({ models }), scan: SCAN,
  sidecars: ['qa-embedder', 'qa-ranker'], rerankers: ['qa-ranker'], ...extra });
del('a chat model: deleted and cleaned up', ['k-chat'], { fetch: [{ ok: true, status: 200, body: { deleted: ['k-chat'] } }], roleRefs: [['fast'], ['smart', 'fast'], []] });
del('two keys sharing ids', ['k-chat', 'k-num', 'k-chat'], { roleRefs: [['fast'], [], ['vision'], []] });
del('a missing key', ['k-missing']);
del('no keys', []);
del('keys not an array', 'k-chat');
del('the system router', ['k-chat', 'k-router']);
del('a laya section', ['k-laya']);
del('the embedding sidecar', ['k-chat', 'k-embed']);
del('the reranker sidecar', ['k-rerank']);
del('sidecar check throws', ['k-chat'], { sidecarThrows: { status: 503, message: 'Could not confirm reranker model protection' } });
del('reranker names throw', ['k-rerank'], { rerankThrows: { status: 503, message: 'Could not confirm reranker model protection' } });
del('embedding protection unknown', ['k-chat'], { embedding: 'unknown' });
del('embedding service absent', ['k-chat'], { embedding: 'none' });
del('embedding service without protectionKnown', ['k-chat'], { embedding: 'legacy' });
del('an empty modelId and null sections', ['k-empty']);
del('a number key', [5]);
del('the forward fails', ['k-chat'], { fetch: [{ ok: false, status: 404, body: { detail: 'qa: not found' } }] });
del('no answer', ['k-chat'], { fetch: ['throw'] });
del('the answer is text', ['k-chat'], { fetch: [{ ok: true, status: 200, body: 'qa deleted' }] });
del('the answer is an array', ['k-chat'], { fetch: [{ ok: true, status: 200, body: ['a', 'b'] }] });
del('the answer has unloaded already', ['k-chat'], { fetch: [{ ok: true, status: 207, body: { unloaded: 'x', a: 1, rolesCleared: 0, '2': 'two' } }] });
del('unload fails, rejects, succeeds', ['k-chat'], { unload: [{ ok: false }, 'reject'], roleRefs: [['fast'], ['fast']] });
del('all unloads fail', ['k-chat'], { unload: [{ ok: false }, { ok: 0 }] });
del('no unload', ['k-chat'], { caps: ALL_CAPS.filter((x) => x !== 'unload') });
del('role references as a string', ['k-nosec'], { roleRefs: ['ab'] });
del('role references not iterable', ['k-nosec'], { roleRefs: [5] });
del('role references throw', ['k-nosec'], { roleRefs: [{ throw: { status: 409, message: 'qa roles busy' } }] });
del('forget throws', ['k-nosec'], { forget: [{ throw: { message: 'qa disk' } }] });
del('role references with objects and duplicates', ['k-chat'], { roleRefs: [['a', { r: 1 }, 'a', null], [{ r: 1 }, null, 7, '7']] });
add('models/delete: cold cache scans first', { method: 'POST', target: T('models/delete'), body: '{"models":["k-chat"]}', sidecars: [],
  fetch: [{ ok: true, status: 200, body: SCAN }, { ok: true, status: 200, body: { deleted: 1 } }] });
add('models/delete: cold cache, scan has the router', { method: 'POST', target: T('models/delete'), body: '{"models":["k-router"]}', fetch: [{ ok: true, status: 200, body: SCAN }] });
for (const [n, answer] of [['fails', 'throw'], ['is not ok', { ok: false, status: 500, body: SCAN }], ['is text', { ok: true, status: 200, body: 'qa' }],
  ['has no models', { ok: true, status: 200, body: {} }], ['models not an array', { ok: true, status: 200, body: { models: 'x' } }], ['body null', { ok: true, status: 200, body: null }],
  ['overflows', { overflow: 200 }], ['is an array', { ok: true, status: 200, body: [1] }]]) {
  add(`models/delete: cold cache, the scan ${n}`, { method: 'POST', target: T('models/delete'), body: '{"models":["k-chat"]}', fetch: [answer] });
}
add('models/delete: cold cache and no keys does not scan', { method: 'POST', target: T('models/delete'), body: '{"models":[]}' });
add('models/delete: cache body without models', { method: 'POST', target: T('models/delete'), body: '{"models":["k-chat"]}', scan: { other: 1 }, fetch: [{ ok: true, status: 200, body: SCAN }, OK] });
add('models/delete: cache models not an array', { method: 'POST', target: T('models/delete'), body: '{"models":["k-chat"]}', scan: { models: { 'k-chat': 1 } }, fetch: [{ ok: true, status: 200, body: SCAN }, OK] });
add('models/delete: a null entry in the scan throws', { method: 'POST', target: T('models/delete'), body: '{"models":["k-zzz"]}', scan: { models: [{ key: 'a' }, null] } });
add('models/delete: a null entry after the match', { method: 'POST', target: T('models/delete'), body: '{"models":["a"]}', scan: { models: [{ key: 'a', modelId: 'qa-a' }, null] } });
add('models/delete: entries that are not objects', { method: 'POST', target: T('models/delete'), body: '{"models":["a"]}', scan: { models: [5, 'a', [], true, { key: 'a', modelId: 'qa-a' }] } });
add('models/delete: sections as a string throws', { method: 'POST', target: T('models/delete'), body: '{"models":["a"]}', scan: { models: [{ key: 'a', modelId: 'qa-a', sections: 'laya' }] } });
add('models/delete: sections as an object throws', { method: 'POST', target: T('models/delete'), body: '{"models":["a"]}', scan: { models: [{ key: 'a', modelId: 'qa-a', sections: { x: 1 } }] } });
add('models/delete: a system modelId skips the sections', { method: 'POST', target: T('models/delete'), body: '{"models":["a"]}', scan: { models: [{ key: 'a', modelId: 'qa-router', sections: 'x' }] } });
add('models/delete: object ids', { method: 'POST', target: T('models/delete'), body: '{"models":["a","a"]}', scan: { models: [{ key: 'a', modelId: { id: 1 }, sections: [{ id: 1 }, 'qa-s', 'qa-s'] }] }, roleRefs: [[], [], [], []] });
add('models/delete: a modelId with toString throws', { method: 'POST', target: T('models/delete'), body: '{"models":["a"]}', scan: { models: [{ key: 'a', modelId: { toString: 1 } }] } });
add('models/delete: null body throws', { method: 'POST', target: T('models/delete'), body: 'null', scan: SCAN });
add('models/delete: invalid JSON', { method: 'POST', target: T('models/delete'), body: '{', scan: SCAN });
add('models/delete: GET is plain', { target: T('models/delete'), scan: SCAN });
add('models/delete: encoded path is the same route', { method: 'POST', path: T('models/d%65lete'), body: '{"models":["k-router"]}', scan: SCAN });
add('models/delete: key null matches key null', { method: 'POST', target: T('models/delete'), body: '{"models":[null]}', scan: { models: [{ key: null, modelId: 'qa-null' }] } });

// sections/<name>.
const SSCAN = { models: [{ key: 'k1', modelId: 'qa-router', sections: ['qa-router-alias'] }, { key: 'k2', modelId: 'qa-embedder', sections: ['qa-embed-preset'] },
  { key: 'k3', modelId: 'qa-chat', sections: ['qa-chat'] }, { key: 'k4', modelId: 'qa-strsec', sections: 'qa-in-a-string' }] };
const sec = (name, method, rest, extra = {}) => add(`sections: ${name}`, { method, path: T(rest), scan: SSCAN, sidecars: ['qa-embedder', 'qa-direct'], rerankers: ['qa-direct'], ...extra });
for (const method of ['DELETE', 'POST', 'PUT', 'GET', 'PATCH']) {
  sec(`${method} the system name`, method, 'sections/qa-router', { body: '{}' });
  sec(`${method} a section of the system model`, method, 'sections/qa-router-alias', { body: '{}' });
  sec(`${method} rename of the system name`, method, 'sections/qa-router/rename', { body: '{}' });
  sec(`${method} a sidecar section`, method, 'sections/qa-embed-preset', { body: '{}' });
  sec(`${method} a sidecar name`, method, 'sections/qa-direct', { body: '{}' });
  sec(`${method} a chat section`, method, 'sections/qa-chat', { body: '{"values":{"model":"/m/qa.gguf"}}' });
}
sec('DELETE embedding protection unknown', 'DELETE', 'sections/qa-chat', { embedding: 'unknown' });
sec('DELETE embedding protection unknown, uncached name', 'DELETE', 'sections/qa-new', { embedding: 'unknown' });
sec('DELETE reranker names throw', 'DELETE', 'sections/qa-direct', { rerankThrows: { status: 503, message: 'Could not confirm reranker model protection' } });
sec('DELETE sidecar check throws', 'DELETE', 'sections/qa-chat', { sidecarThrows: { status: 503, message: 'Could not confirm reranker model protection' } });
sec('DELETE a sections string matches by substring', 'DELETE', 'sections/in-a', { sidecars: ['qa-strsec'] });
sec('DELETE no cache', 'DELETE', 'sections/qa-chat', { scan: undefined });
sec('DELETE cache with a null entry throws', 'DELETE', 'sections/qa-chat', { scan: { models: [null] } });
sec('DELETE cache with a numeric sections throws', 'DELETE', 'sections/qa-chat', { scan: { models: [{ sections: 5 }] } });
sec('DELETE cache with object sections throws', 'DELETE', 'sections/qa-chat', { scan: { models: [{ sections: { a: 1 } }] } });
sec('DELETE a system role name, encoded unicode', 'DELETE', 'sections/qa-sys%20%E2%9C%93');
sec('DELETE a laya name', 'DELETE', 'sections/Laya-QA');
sec('DELETE a trailing-space name', 'DELETE', 'sections/qa-direct%20');
sec('POST safe-defaults', 'POST', 'sections/qa-chat/safe-defaults', { body: '{}' });
sec('POST safe-defaults of the router', 'POST', 'sections/qa-router/safe-defaults', { body: '{}' });
sec('GET autoconfig of the router', 'GET', 'sections/qa-router/autoconfig');
const put = (name, values, extra = {}) => sec(`PUT ${name}`, 'PUT', 'sections/qa-chat', { body: JSON.stringify({ values, ...(extra.extras === undefined ? {} : { extras: extra.extras }) }), ...extra });
put('a model path', { model: '/m/qa.gguf', 'ctx-size': '4096' });
put('model null', { model: null });
put('model empty', { model: '' });
put('model blank', { model: ' \t' });
put('model number', { model: 5 });
put('-m number', { '-m': 5 });
put('--model object', { '--model': {} });
put('LLAMA_ARG_MODEL array', { LLAMA_ARG_MODEL: [] });
put('  m  null', { '  m  ': null });
put('---m blank', { '---m': '' });
put('mmproj empty is allowed', { mmproj: '' });
put('mmproj null is allowed', { mmproj: null });
put('mm number', { mm: 1 });
put('md bool', { md: true });
put('model-draft number', { 'model-draft': 0 });
put('LLAMA_ARG_MMPROJ number', { LLAMA_ARG_MMPROJ: 2 });
put('LLAMA_ARG_MODEL_DRAFT number', { LLAMA_ARG_MODEL_DRAFT: 2 });
put('a case-changed alias is not an alias', { Model: 5, M: 5 });
put('constructor number', { constructor: 5 });
put('toString number', { toString: 5 });
put('hasOwnProperty null', { hasOwnProperty: null });
put('valueOf bool', { valueOf: false });
put('__proto__ number', JSON.parse('{"__proto__":5}'));
put('__defineGetter__ object', { __defineGetter__: {} });
put('isPrototypeOf, propertyIsEnumerable, toLocaleString', { isPrototypeOf: 1, propertyIsEnumerable: 1, toLocaleString: 1 });
put('__lookupSetter__', { ' --__lookupSetter__ ': 1 });
put('__defineSetter__ and __lookupGetter__', { __defineSetter__: 1 });
put('__lookupGetter__', { '-__lookupGetter__': [] });
put('values as an array', ['/m', 5]);
put('values null', null);
put('values a string', 'model=5');
put('numeric keys come first', { model: '/m', 10: 'x', 2: 5 });
put('the second refusal wins only in order', { mm: 1, model: null });
put('a non-path number is fine', { 'ctx-size': 4096 });
add('sections: PUT values not JSON', { method: 'PUT', path: T('sections/qa-chat'), body: '{"values":', scan: SSCAN });
add('sections: PUT null body throws with presetRefusal', { method: 'PUT', path: T('sections/qa-chat'), body: 'null', scan: SSCAN });
add('sections: PUT null body without presetRefusal', { method: 'PUT', path: T('sections/qa-chat'), body: 'null', scan: SSCAN, caps: ALL_CAPS.filter((x) => x !== 'presetRefusal') });
add('sections: PUT empty body', { method: 'PUT', path: T('sections/qa-chat'), body: '', scan: SSCAN });
add('sections: PUT number out of range goes to core', { method: 'PUT', path: T('sections/qa-chat'), body: '{"values":{"model":1e400}}', scan: SSCAN });
add('sections: PUT rename skips the path checks', { method: 'PUT', path: T('sections/qa-chat/rename'), body: '{"values":{"model":5}}', scan: SSCAN });
const refusalPut = (name, refusal, extra = {}) => sec(`PUT preset refusal: ${name}`, 'PUT', 'sections/qa-chat', { body: '{"values":{"model":"/m/qa.gguf","ctx-size":"8192"}}', presetRefusal: refusal, ...extra });
refusalPut('none', null);
refusalPut('a budget refusal', { error: 'qa: does not fit', code: 'over_budget', needGib: 30 });
refusalPut('an invalid size', { error: 'qa: invalid size', code: 'invalid_size' });
refusalPut('a string refusal', 'qa no');
refusalPut('a zero refusal is none', 0);
refusalPut('throws', { throw: { message: 'qa: presets unreadable' } });
refusalPut('throws a 4xx', { throw: { status: 422, message: 'qa: bad preset' } });
refusalPut('absent', null, { caps: ALL_CAPS.filter((x) => x !== 'presetRefusal') });
const extras = (name, extrasValue, values = { model: '/m/qa.gguf' }) => sec(`PUT extras: ${name}`, 'PUT', 'sections/qa-chat', { body: JSON.stringify({ values, extras: extrasValue }) });
extras('lines', 'ctx-size = 4096\n  # comment = 1\n; also = 2\n=x\n  temp=0.7  \nnoequals\n flash-attn =  \nkey with space = v a l \n');
extras('CRLF and unicode spaces', 'a = 1\r\nb =　two \nc = x\ry\n﻿d = 4\n e=5');
extras('overrides values', 'model = /m/other.gguf\nctx-size=1', { model: '/m/qa.gguf', 'ctx-size': '2', n: 5, e: '' });
extras('numeric keys', '7 = seven\n3 = three\nz = zed', { 5: 'five', a: 'A' });
extras('__proto__ lines', '__proto__ = x\nconstructor = y');
extras('__proto__ own key is set', '__proto__ = x', JSON.parse('{"__proto__":"own","model":"/m"}'));
extras('extras a number', 42);
extras('extras an array', ['a=1', 'b=2']);
extras('extras null', null);
extras('extras object with toString throws', { toString: 'x' });
extras('values with non-strings are dropped', '', { a: 1, b: null, c: '', d: 'ok', e: { x: 1 }, f: [] });
extras('values as an array', 'x=1', ['p', 'q', '']);
extras('a key ending in an equals sign', 'a==b\n==c\n#=1\nk =\n');

// Preset writes and their hold.
const pw = (name, method, rest, extra = {}) => add(`preset write: ${name}`, { method, path: T(rest), body: method === 'DELETE' ? undefined : '{"values":{"model":"/m/qa.gguf"}}', ...extra });
for (const [method, rest] of [['PUT', 'models-ini'], ['PUT', 'idle-unload'], ['PUT', 'sections/qa-chat'], ['DELETE', 'sections/qa-chat'], ['POST', 'sections/qa-chat/safe-defaults'],
  ['POST', 'sections/qa-chat/rename'], ['POST', 'models-ini'], ['GET', 'models-ini'], ['PUT', 'sections/qa-chat/rename'], ['DELETE', 'sections/qa-chat/rename'],
  ['POST', 'sections/qa-chat'], ['PUT', 'sections/qa-chat/autoconfig'], ['PATCH', 'models-ini'], ['PUT', 'models-ini/x'], ['PUT', 'idle-unload/']]) {
  pw(`${method} ${rest}`, method, rest);
}
pw('refused', 'PUT', 'models-ini', { writer: 'refuse' });
pw('refused DELETE', 'DELETE', 'sections/qa-chat', { writer: 'refuse' });
pw('the hold throws', 'PUT', 'models-ini', { writer: { throwBefore: { status: 503, message: 'The model service could not confirm that chat is idle, so nothing was changed. Try again shortly.' } } });
pw('the hold throws a 4xx', 'PUT', 'models-ini', { writer: { throwBefore: { status: 423, message: 'qa locked' } } });
pw('the reload loses the lease', 'PUT', 'models-ini', { writer: { reloadThrows: { status: 503, message: 'qa lease lost' } } });
pw('the reload loses the lease, 4xx', 'PUT', 'models-ini', { writer: { reloadThrows: { status: 409, message: 'qa lease lost' } } });
pw('no reload when the write failed', 'PUT', 'models-ini', { fetch: [{ ok: false, status: 409, body: { error: 'qa revision' } }] });
pw('no reload when preset failed', 'PUT', 'idle-unload', { fetch: [{ ok: true, status: 200, body: { preset: 'failed' } }] });
pw('reload when preset written', 'PUT', 'idle-unload', { fetch: [{ ok: true, status: 200, body: { preset: 'written' } }] });
pw('reload when body is text', 'PUT', 'idle-unload', { fetch: [{ ok: true, status: 200, body: 'failed' }] });
pw('no answer', 'PUT', 'models-ini', { fetch: ['throw'] });
pw('no answer and the reload throws', 'PUT', 'models-ini', { fetch: ['throw'], writer: { reloadThrows: { message: 'x' } } });
pw('no writer: write then reload', 'PUT', 'models-ini', { caps: ALL_CAPS.filter((x) => x !== 'writePresets') });
pw('no writer: reload rejects', 'PUT', 'models-ini', { caps: ALL_CAPS.filter((x) => x !== 'writePresets'), reloadRejects: true });
pw('no writer: no reload when failed', 'PUT', 'models-ini', { caps: ALL_CAPS.filter((x) => x !== 'writePresets'), fetch: [{ ok: true, status: 200, body: { preset: 'failed' } }] });
pw('no writer, no reloader', 'PUT', 'models-ini', { caps: ALL_CAPS.filter((x) => x !== 'writePresets' && x !== 'reloadPresets') });
pw('no writer: no answer', 'DELETE', 'sections/qa-chat', { caps: ALL_CAPS.filter((x) => x !== 'writePresets'), fetch: ['throw'] });

// autoconfig and its budget.
const ac = (name, search, budget, extra = {}) => add(`autoconfig: ${name}`, { path: T('sections/qa-chat/autoconfig'), search, budget, ...extra });
ac('a budget', '', 12);
ac('replaces the client budget', '?budget_gib=99&ctx=8192', 12.5);
ac('two client budgets', '?budget_gib=1&budget_gib=2&a=b', 8);
ac('zero removes it', '?budget_gib=99', 0);
ac('zero and nothing else', '?budget_gib=99', 0);
ac('negative', '?x=1', -1);
ac('null', '?x=1', null);
ac('undefined', '?x=1', undefined);
ac('a string number', '?x=1', '16');
ac('a string', '?x=1', 'lots');
ac('huge', '', 1e21);
ac('tiny', '', 1e-7);
ac('true', '', true);
ac('reencodes the query', '?q=a%20b+c&e=%C3%A9&s=%2B&t=*-._~!\'()&u=%zz&&v&=w', 4);
ac('a lone question mark', '?', 4);
ac('a bare key', '?flag', 4);
ac('no sizing', '?budget_gib=3', 12, { caps: ALL_CAPS.filter((x) => x !== 'sizingBudgetGib') });
ac('POST is not rewritten', '?budget_gib=3', 12, { method: 'POST', body: '{}' });
ac('not autoconfig', '?budget_gib=3', 12, { path: T('sections/qa-chat/autoconfigx') });
ac('encoded path', '?budget_gib=3', 12, { path: T('sections/qa%2Dchat/autoc%6Fnfig') });
ac('nested name is not autoconfig', '?budget_gib=3', 12, { path: T('sections/a/b/autoconfig') });
add('autoconfig: through the URL parser', { target: T('sections/qa chat/autoconfig?budget_gib=3&n=é ✓#frag'), budget: 6 });

// The answer.
const ans = (name, answer, extra = {}) => add(`answer: ${name}`, { path: T('benchmark/runs'), fetch: [answer], ...extra });
ans('ok object', { ok: true, status: 200, body: { runs: [1, 2] } });
ans('ok text', { ok: true, status: 200, body: 'qa text' });
ans('ok empty text', { ok: true, status: 200, body: '' });
ans('ok number', { ok: true, status: 200, body: 5 });
ans('ok zero', { ok: true, status: 200, body: 0 });
ans('ok true', { ok: true, status: 200, body: true });
ans('ok null', { ok: true, status: 200, body: null });
ans('ok array', { ok: true, status: 200, body: [1, 'a'] });
ans('ok 204 without body', { ok: true, status: 204 });
ans('not ok with detail', { ok: false, status: 422, body: { detail: 'qa: bad', error: 'qa: other' } });
ans('not ok with a detail object', { ok: false, status: 422, body: { detail: [{ loc: ['body'], msg: 'qa' }] } });
ans('not ok with error', { ok: false, status: 409, body: { error: 'qa: conflict' } });
ans('not ok with neither', { ok: false, status: 500, body: { message: 'qa' } });
ans('not ok text', { ok: false, status: 502, body: 'Bad Gateway' });
ans('not ok empty', { ok: false, status: 503, body: '' });
ans('not ok array', { ok: false, status: 400, body: ['qa'] });
ans('not ok falsy detail', { ok: false, status: 400, body: { detail: 0, error: '' } });
ans('overflow', { overflow: 200 });
ans('overflow 500', { overflow: 500 });
ans('no answer', 'throw');
ans('a 3xx', { ok: false, status: 304, body: '' });
ans('lone surrogate text', { ok: false, status: 500, body: 'qa \ud83d' });

// Benchmark evidence on GET benchmark/runs/<n>.
const NOWMS = 1_800_000_000_000;
const RUN = (o = {}) => ({ run: { id: 12, status: 'done', finished_at: NOWMS / 1000 - 60, ...o.run },
  variants: o.variants ?? [{ alias: 'qa-a', argv_json: '{}' }, { alias: 'qa-b', argv_json: '{"ctx":1}' }, { alias: 'qa-c' }],
  results: o.results ?? [
    { alias: 'qa-a', gen_tps: 10.04, prompt_tps: 100 }, { alias: 'qa-a', gen_tps: 12.06, prompt_tps: 0 }, { alias: 'qa-a', gen_tps: 11, prompt_tps: 120.25, cold: true },
    { alias: 'qa-b', gen_tps: 5 }, { alias: 'qa-b', gen_tps: 6 },
    { alias: 'qa-c', gen_tps: '7.5' }, { alias: 'qa-c', gen_tps: 8, err: 'x' }, { alias: 'qa-c', gen_tps: 9.25, prompt_tps: '50' }] });
const ev = (name, body, extra = {}) => add(`evidence: ${name}`, { path: T('benchmark/runs/12'), now: NOWMS, fetch: [{ ok: true, status: 200, body }], ...extra });
ev('records the plain variants', RUN());
ev('not ok', RUN(), { fetch: [{ ok: false, status: 500, body: RUN() }] });
ev('no recordEvidence', RUN(), { caps: ALL_CAPS.filter((x) => x !== 'recordEvidence') });
ev('the run is old', RUN({ run: { finished_at: NOWMS / 1000 - 1801 } }));
ev('the run is just in time', RUN({ run: { finished_at: NOWMS / 1000 - 1800 } }));
ev('not done', RUN({ run: { status: 'running' } }));
ev('finished_at text', RUN({ run: { finished_at: String(NOWMS / 1000 - 5) } }));
ev('variants not iterable throws', { ...RUN(), variants: 5 });
ev('results a string throws', { ...RUN(), results: 'x' });
ev('a null variant throws', { ...RUN(), variants: [null] });
add('evidence: runs/x is not a run', { path: T('benchmark/runs/x'), now: NOWMS, fetch: [{ ok: true, status: 200, body: RUN() }] });
add('evidence: POST runs/12 is not recorded', { method: 'POST', path: T('benchmark/runs/12'), now: NOWMS, body: '{}', fetch: [{ ok: true, status: 200, body: RUN() }] });
add('evidence: an encoded run number', { path: T('benchmark/runs/%31%32'), now: NOWMS, fetch: [{ ok: true, status: 200, body: RUN() }] });
add('evidence: a unicode digit is not a run', { path: T('benchmark/runs/%D9%A1'), now: NOWMS, fetch: [{ ok: true, status: 200, body: RUN() }] });
add('evidence: HEAD is not recorded', { method: 'HEAD', path: T('benchmark/runs/12'), now: NOWMS, fetch: [{ ok: true, status: 200, body: RUN() }] });

// ── Seeded random requests ──────────────────────────────────────────────────────────────────
const RAND_SEGMENTS = ['models', 'delete', 'sections', 'qa-chat', 'qa-router', 'qa-embedder', 'rename', 'autoconfig', 'safe-defaults', 'benchmark', 'start', 'sweep',
  'runs', '12', 'models-ini', 'idle-unload', '%2e', '%2F', '%E2%9C%93', '%C3', 'a b', '..', '.', '', '%25', 'Laya-x', 'qa-direct', '%71a-chat', 'é'];
const RAND_BODIES = ['', '{}', 'null', '{nope', '{"aliases":["qa-chat","qa-embed-x"]}', '{"aliases":["qa-a","qa-b"]}', '{"models":["k-chat"]}', '{"models":["k-router"]}',
  '{"models":["k-embed"]}', '{"values":{"model":5}}', '{"values":{"model":"/m"},"extras":"a=1"}', '{"values":{"mm":null}}', '[]', '"x"'];
const RAND_FETCH = [OK, 'throw', { ok: false, status: 409, body: { detail: 'qa' } }, { ok: true, status: 200, body: SCAN }, { ok: true, status: 200, body: 'qa' }, { overflow: 200 },
  { ok: true, status: 200, body: { preset: 'failed' } }];
for (let n = 0; n < 400; n += 1) {
  const depth = 1 + Math.floor(rand() * 4);
  const rest = Array.from({ length: depth }, () => pick(RAND_SEGMENTS)).join('/');
  const method = pick(['GET', 'POST', 'PUT', 'DELETE', 'GET', 'POST', 'HEAD', 'PATCH']);
  add(`random ${n}`, clean({
    method, path: T(rest), search: pick(['', '', '?budget_gib=2', '?a=1&b=2']), role: rand() < 0.95 ? 'admin' : 'member',
    body: pick(RAND_BODIES), scan: rand() < 0.6 ? SCAN : undefined, catalogue: CAT,
    sidecars: ['qa-embedder', 'qa-direct'], rerankers: pick([[], ['qa-direct']]),
    embedding: pick(['known', 'known', 'known', 'unknown', 'none']),
    fetch: [pick(RAND_FETCH), pick(RAND_FETCH)],
    loadRefusals: rand() < 0.2 ? { 'qa-b': { error: 'qa: too big' } } : undefined,
    presetRefusal: pick([null, null, { error: 'qa', code: 'invalid_size' }, { error: 'qa', code: 'over' }]),
    writer: pick(['grant', 'grant', 'refuse', { reloadThrows: { status: 503, message: 'qa lease' } }]),
    sweepHold: pick(['grant', { throw: { status: 503, code: 'maintenanceAuthorityUnavailable' } }, { reject: { status: 409 } }]),
    budget: pick([12, 0, null]),
    caps: rand() < 0.8 ? undefined : ALL_CAPS.filter(() => rand() < 0.7),
    env: rand() < 0.95 ? undefined : pick([{}, { url: 'http://qa-loader.invalid' }]),
  }));
}

// ── throughputRecords directly ──────────────────────────────────────────────────────────────
const EV = [];
const evd = (detail, now = NOWMS) => EV.push({ detail, now });
evd(RUN());
evd(undefined); evd(null); evd(5); evd('x'); evd({}); evd({ run: null }); evd({ run: { status: 'done' } });
for (const finished of [null, true, false, '', ' ', '0x10', '1e3', 'Infinity', '-Infinity', 'NaN', [], [5], ['7'], [1, 2], {}, '  1799999940 ', '1799999940.', '.5', '+1799999940', '1_0', '0b11', '0o7', '0X1F', '-0x10']) {
  evd({ ...RUN(), run: { id: 'r', status: 'done', finished_at: finished } }, 1800 * 1000);
}
for (const finished of [NOWMS / 1000 - 1800, NOWMS / 1000 - 1800.001, NOWMS / 1000 + 10]) evd({ ...RUN(), run: { id: 1, status: 'done', finished_at: finished } });
for (const argv of [undefined, null, '', '{}', '[]', '[1]', 'null', '0', '5', 'true', '"ab"', '""', '{"a":1}', '{bad', 7, true, {}, [], [1], { toString: 'x' }, ' {} ']) {
  evd({ run: { id: 3, status: 'done', finished_at: NOWMS / 1000 }, variants: [{ alias: 'qa-v', argv_json: argv }], results: [{ alias: 'qa-v', gen_tps: 3 }, { alias: 'qa-v', gen_tps: 4 }] });
}
for (const r of [{ gen_tps: 0 }, { gen_tps: -1 }, { gen_tps: '' }, { gen_tps: null }, { gen_tps: [2] }, { gen_tps: '0x2' }, { gen_tps: 1e308 }, { gen_tps: 'Infinity' }, { gen_tps: 2, truncated: 1 },
  { gen_tps: 2, contended: 'yes' }, { gen_tps: 2, cold: 0 }, { gen_tps: 2, err: '' }, { gen_tps: 2, prompt_tps: -3 }, { gen_tps: 2.25, prompt_tps: 0.04 }, { gen_tps: { toString: 1 } }]) {
  evd({ run: { id: 4, status: 'done', finished_at: NOWMS / 1000 }, variants: [{ alias: 'qa-r' }], results: [{ alias: 'qa-r', gen_tps: 1.05, prompt_tps: 2.35 }, { alias: 'qa-r', ...r }] });
}
evd({ run: { id: 5, status: 'done', finished_at: NOWMS / 1000 }, variants: 'ab', results: [{ gen_tps: 1 }, { gen_tps: 2 }] });
evd({ run: { id: 5, status: 'done', finished_at: NOWMS / 1000 }, variants: [{}], results: [{ gen_tps: 1 }, { gen_tps: 2 }, { alias: null, gen_tps: 3 }] });
evd({ run: { id: 5, status: 'done', finished_at: NOWMS / 1000 }, variants: [{ alias: 'x' }], results: [{ alias: 'x', gen_tps: 1 }, null] });
evd({ run: { id: 5, status: 'done', finished_at: NOWMS / 1000 }, variants: [{ alias: 'x' }], results: null });
evd({ run: { id: 5, status: 'done', finished_at: NOWMS / 1000 }, variants: null, results: [] });
evd({ run: { status: 'done', finished_at: NOWMS / 1000 }, variants: [{ alias: 'x' }, { alias: 'x' }], results: [{ alias: 'x', gen_tps: 0.05 }, { alias: 'x', gen_tps: 0.15 }, { alias: 'x', gen_tps: 0.25 }] });
evd({ run: { id: { n: 1 }, status: 'done', finished_at: NOWMS / 1000 }, variants: [{ alias: 7 }], results: [{ alias: 7, gen_tps: 2.45 }, { alias: 7, gen_tps: 2.55 }, { alias: 7, gen_tps: 1e308 }, { alias: 7, gen_tps: 1e308 }] });
for (let n = 0; n < 300; n += 1) {
  const aliases = ['qa-a', 'qa-b', 'qa-c'];
  const variants = Array.from({ length: Math.floor(rand() * 4) }, () => clean({ alias: pick(aliases), argv_json: pick([undefined, '{}', '', '{"x":1}', 'null', '[]']) }));
  const results = Array.from({ length: Math.floor(rand() * 9) }, () => clean({ alias: pick(aliases), gen_tps: pick([0, 1.5, 2.25, 3.35, 10, '4', -1, undefined, 7.05, 100.95]),
    prompt_tps: pick([undefined, 0, 50.5, 120.15, '33', -2]), cold: rand() < 0.1 || undefined, err: rand() < 0.1 ? 'qa' : undefined, contended: rand() < 0.05 || undefined }));
  evd({ run: { id: n, status: pick(['done', 'done', 'done', 'failed']), finished_at: NOWMS / 1000 - Math.floor(rand() * 2400) }, variants, results });
}
const evidence = EV.map(({ detail, now }) => {
  const d = viaJson(detail);
  try { return { ...clean({ detail: d }), now, out: throughputRecords(d, { now }) }; } catch (err) { return { ...clean({ detail: d }), now, threw: errorResponse(err).status }; }
});

(async () => {
  const cases = [];
  for (const raw of S) {
    const c = viaJson(raw);
    const { p, search, expect } = await runCase(c);
    cases.push({ ...c, p, search, expect });
  }
  fs.rmSync(rolesDir, { recursive: true, force: true });
  const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
  const text = `{"version":1,"generator":"crates/model-manager-proxy/tests/record_model_manager_proxy.cjs",\n"node":${JSON.stringify(process.version)},\n`
    + `"sources":${JSON.stringify(sources)},\n"defaults":${JSON.stringify({ caps: ALL_CAPS, env: DEFAULT_ENV, fetch: [OK], now: 1_800_000_000_000 })},\n`
    + `"cases":[\n${lines(cases)}\n],\n"evidence":[\n${lines(evidence)}\n]}\n`;
  const statuses = {};
  for (const c of cases) statuses[c.expect.status ?? 'pass'] = (statuses[c.expect.status ?? 'pass'] || 0) + 1;
  const counts = `${cases.length} requests (${Object.entries(statuses).map(([k, v]) => `${v}×${k}`).join(' ')}; ${cases.filter((c) => c.expect.threw).length} thrown), ${evidence.length} evidence`;
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
