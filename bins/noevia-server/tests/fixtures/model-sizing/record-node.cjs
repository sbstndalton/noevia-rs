'use strict';
// Records noevia-core's own answers for GET /api/models/estimate and GET /api/models/preset/suggest
// (sbstndalton/noevia#2189) over synthetic fixtures, for tests/model_sizing.rs to replay against the
// native front byte for byte. Synthetic data only: generated GGUF headers (sparse files), a generated
// models.ini and a fake llama.cpp router listing.
//
//   node record-node.cjs <noevia-core checkout at the last Core that answers both GETs> > cases.json
//
// The checkout needs server/wasm/dav-parse.wasm (built from this repository). The cases were recorded
// at noevia-core fbac2e4 (the batch M1 head, the last Core with both handlers). Every budget here is at
// most 8 GiB, so the host-RAM ceiling (host RAM - 4 GiB) does not change an answer on a 16 GiB runner.
const fs = require('node:fs'), os = require('node:os'), path = require('node:path'), http = require('node:http');
const core = path.resolve(process.argv[2] || '.');
const req = (m) => require(path.join(core, 'server', m));
const { createModelManager } = req('model-manager.cjs');
const { createModelRoutes } = req('routes/models.cjs');
const { json, fetchJson, errorResponse } = req('http.cjs');
const { createInferenceBudget, cacheRamLimits } = req('inference-budget.cjs');

const GIB = 1024 ** 3;
// ── Synthetic GGUF v3 headers (no tensors) ─────────────────────────────────────────────────────
function gguf(kv, version = 3) {
  const parts = [Buffer.from('GGUF'), u32(version), u64(0), u64(Object.keys(kv).length)];
  for (const [key, [type, value]] of Object.entries(kv)) parts.push(str(key), u32(type), val(type, value));
  return Buffer.concat(parts);
  function u32(n) { const b = Buffer.alloc(4); b.writeUInt32LE(n); return b; }
  function u64(n) { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b; }
  function str(s) { const b = Buffer.from(s); return Buffer.concat([u64(b.length), b]); }
  function val(type, v) {
    if (type === 4) return u32(v);
    if (type === 10) return u64(v);
    if (type === 7) return Buffer.from([v ? 1 : 0]);
    if (type === 8) return str(v);
    if (type === 9) { const [sub, items] = v; return Buffer.concat([u32(sub), u64(items.length), ...items.map((x) => val(sub, x))]); }
    throw Error('type');
  }
}
const llama = (arch, ctx, extra = {}) => ({
  'general.architecture': [8, arch], 'general.name': [8, 'Synthetic ' + arch], [`${arch}.context_length`]: [ctx > 0xffffffff ? 10 : 4, ctx],
  [`${arch}.embedding_length`]: [4, 4096], [`${arch}.block_count`]: [4, 32], [`${arch}.attention.head_count`]: [4, 32],
  [`${arch}.attention.head_count_kv`]: [4, 8], 'tokenizer.chat_template': [8, '{{ messages }}'], ...extra,
});
const header = {
  good: gguf(llama('llama', 32768)),
  vision: gguf(llama('gemma3', 131072, { 'gemma3.attention.sliding_window': [4, 1024] })),
  swa: gguf({ ...llama('gemma4', 131072), 'gemma4.attention.sliding_window': [4, 512], 'gemma4.attention.sliding_window_pattern': [9, [7, Array.from({ length: 32 }, (_, i) => i % 6 !== 5)]], 'gemma4.attention.key_length_swa': [4, 64], 'gemma4.attention.value_length_swa': [4, 64] }),
  hybrid: gguf({ 'general.architecture': [8, 'qwen35'], 'qwen35.context_length': [4, 262144], 'qwen35.embedding_length': [4, 4096], 'qwen35.block_count': [4, 32], 'qwen35.attention.head_count': [4, 16], 'qwen35.attention.head_count_kv': [4, 4], 'qwen35.attention.key_length': [4, 256], 'qwen35.attention.value_length': [4, 256], 'qwen35.full_attention_interval': [4, 4], 'qwen35.ssm.state_size': [4, 128], 'qwen35.nextn_predict_layers': [4, 1], 'tokenizer.chat_template': [8, '{{ messages }}'] }),
  embed: gguf({ 'general.architecture': [8, 'nomic-bert'], 'nomic-bert.context_length': [4, 2048], 'nomic-bert.embedding_length': [4, 768], 'nomic-bert.block_count': [4, 12], 'nomic-bert.attention.head_count': [4, 12] }),
  mystery: gguf({ 'general.architecture': [8, 'mystery'], 'tokenizer.chat_template': [8, 'x'] }),
  huge: gguf(llama('llama', 2 ** 60)),
  bigpattern: gguf({ ...llama('gemma4', 131072), 'gemma4.attention.sliding_window': [4, 512], 'gemma4.attention.sliding_window_pattern': [9, [7, Array.from({ length: 2000 }, (_, i) => i % 6 !== 5)]] }),
  perlayer: gguf({ ...llama('llama', 65536), 'llama.attention.head_count_kv': [9, [4, Array.from({ length: 32 }, (_, i) => (i % 2 ? 8 : 0))]] }),
  notgguf: Buffer.from('NOPE, not a model file at all'),
  v1: Buffer.concat([Buffer.from('GGUF'), Buffer.from([1, 0, 0, 0]), Buffer.alloc(16)]),
};
header.trunc = header.good.subarray(0, 60);
const b64 = (b) => Buffer.from(b).toString('base64');
// Every file of the model mount (sizes are sparse: only the header is written).
const MODEL_FILES = [
  { path: 'models/good.gguf', b64: b64(header.good), size: Math.round(4.5 * GIB) },
  { path: 'models/vision.gguf', b64: b64(header.vision), size: Math.round(7.2 * GIB) },
  { path: 'models/vision-mmproj.gguf', b64: b64(Buffer.from('projector')), size: Math.round(0.8 * GIB) },
  { path: 'models/swa.gguf', b64: b64(header.swa), size: Math.round(3.1 * GIB) },
  { path: 'models/hybrid.gguf', b64: b64(header.hybrid), size: Math.round(2.9 * GIB) },
  { path: 'models/embed.gguf', b64: b64(header.embed), size: 300 * 1024 * 1024 },
  { path: 'models/mystery.gguf', b64: b64(header.mystery), size: GIB },
  { path: 'models/huge.gguf', b64: b64(header.huge), size: 2 * GIB },
  { path: 'models/bigpattern.gguf', b64: b64(header.bigpattern), size: 3 * GIB },
  { path: 'models/perlayer.gguf', b64: b64(header.perlayer), size: 5 * GIB },
  { path: 'models/big.gguf', b64: b64(header.good), size: 40 * GIB },
  { path: 'models/notgguf.gguf', b64: b64(header.notgguf), size: header.notgguf.length },
  { path: 'models/v1.gguf', b64: b64(header.v1), size: header.v1.length },
  { path: 'models/trunc.gguf', b64: b64(header.trunc), size: header.trunc.length },
  { path: 'models/sub/nested.gguf', b64: b64(header.good), size: GIB },
  { path: 'cache/cached.gguf', b64: b64(header.hybrid), size: Math.round(1.5 * GIB) },
  { path: 'outside.gguf', b64: b64(header.good), size: GIB },
  { path: 'models/link.gguf', symlink: '../outside.gguf' },
  { path: 'models/inner-link.gguf', symlink: 'good.gguf' },
];
const INI = [
  '; synthetic presets (sbstndalton/noevia#2189)',
  'version = 1',
  '[*]',
  'cache-ram = 512',
  'flash-attn = on',
  '',
  '[good]',
  'model = /models/good.gguf',
  'ctx-size = 16384',
  'LLAMA_ARG_N_PARALLEL = 2',
  'unknown-key = dropped',
  '[vision]',
  'model = /models/vision.gguf',
  'mmproj = /models/vision-mmproj.gguf',
  'c = 8192',
  '[missing-projector]',
  'model = /models/good.gguf',
  'mmproj = /models/nope-mmproj.gguf',
  '[escape]',
  'model = /models/../outside.gguf',
  '[link]',
  'model = /models/link.gguf',
  '[inner]',
  'model = /models/sub/../inner-link.gguf',
  '[foreign]',
  'model = /elsewhere/x.gguf',
  '[notgguf]',
  'model = /models/notgguf.gguf',
  '[v1]',
  'm = /models/v1.gguf',
  '[trunc]',
  'model = /models/trunc.gguf',
  '[swa]',
  'model = /models/swa.gguf',
  'cache-type-k = q8_0',
  'ctv = q4_0',
  '[hybrid]',
  'model = /models/hybrid.gguf',
  'spec-type = draft-mtp',
  '[embed]',
  'model = /models/embed.gguf',
  '[mystery]',
  'model = /models/mystery.gguf',
  '[huge]',
  'model = /models/huge.gguf',
  '[bigpattern]',
  'model = /models/bigpattern.gguf',
  '[perlayer]',
  'model = /models/perlayer.gguf',
  '[big]',
  'model = /models/big.gguf',
  '[nested]',
  'model = /models/sub/nested.gguf',
  '[dir]',
  'model = /models/',
  '[badctx]',
  'model = /models/good.gguf',
  'ctx-size = -1',
  '[ünï cödé]',
  'model = /models/good.gguf',
  '  [spaces]   ; a comment',
  '   model   =   /models/good.gguf   ',
  'batch-size = 512',
  'ubatch-size=1024',
  'cram = 256',
  '[later]',
  'model = /models/good.gguf',
  'ctx-size = 4096',
  'ctx-size = 12288',
  '[crlf]\r',
  'model = /models/good.gguf\r',
  'ctx-size = 8192\r',
  '',
].join('\n');
const LISTING = {
  data: [
    { id: 'good', status: { value: 'loaded', args: ['--model', '/models/good.gguf', '--ctx-size', '16384'] } },
    { id: 'routed', status: { value: 'unloaded', args: ['-m', '/models/vision.gguf', '-mm', '/models/vision-mmproj.gguf'] } },
    { id: 'cached', status: { value: 'unloaded', args: ['--model', '/cache/cached.gguf'] } },
    { id: 'weird', status: { value: 'unloaded', args: 'oops' } },
    { id: 'argnum', status: { value: 'unloaded', args: ['--model', 5] } },
    { id: 'argempty', status: { value: 'unloaded', args: ['--model', ''] } },
    { id: 'nullstatus', status: null },
    { id: 'vision', status: { value: 'unloaded', args: ['--model', '/models/vision.gguf', '--mmproj', '/models/missing.gguf'] } },
  ],
};
const MODELS = ['good', 'vision', 'missing-projector', 'escape', 'link', 'inner', 'foreign', 'notgguf', 'v1', 'trunc', 'swa', 'hybrid', 'embed', 'mystery', 'huge', 'bigpattern', 'perlayer', 'big', 'nested', 'dir', 'badctx', 'ünï cödé', 'spaces', 'later', 'crlf', 'routed', 'cached', 'weird', 'argnum', 'argempty', 'nullstatus', 'absent'];
const q = (m) => '?model=' + encodeURIComponent(m);
const both = (queries) => queries.flatMap((query) => [{ route: 'estimate', query }, { route: 'preset/suggest', query }]);
const ALL = both(MODELS.map(q));
const EDGES = both(['', '?model=', '?other=good', '?model=good&model=vision', '?model=go+od', '?model=%E2%82%AC', '?model=%FF', '?model=' + 'x'.repeat(200), '?model=' + 'x'.repeat(201), '?model=' + '€'.repeat(200), '?model=' + '😀'.repeat(101)]);
const ROLES = both([q('good')]).map((r) => ({ ...r, role: 'member' }));
const BASE_ENV = { LLAMACPP_PRESET_PATH: '$ROOT/models.ini', LLAMACPP_MODELS_PATH: '$ROOT/models', LLAMACPP_CACHE_PATH: '$ROOT/cache' };
const SMALL = both(['good', 'vision', 'swa', 'hybrid'].map(q));
const scenarios = [
  { name: 'main', env: BASE_ENV, budgetSetting: '{"budgetGib":6}', requests: [...ALL, ...EDGES, ...ROLES] },
  { name: 'explicit-lower', env: { ...BASE_ENV, LLAMACPP_AUTOCONFIG_MEMORY_GIB: '3.5' }, budgetSetting: '{"budgetGib":8}', requests: SMALL },
  { name: 'explicit-higher', env: { ...BASE_ENV, LLAMACPP_AUTOCONFIG_MEMORY_GIB: '12' }, budgetSetting: '{"budgetGib":4.5}', requests: SMALL },
  { name: 'minimum-budget', env: { ...BASE_ENV, LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB: '4000', LLAMACPP_CACHE_RAM_HARD_MAX_MIB: '3000' }, budgetSetting: '{"budgetGib":1}', requests: both(['good', 'big', 'hybrid'].map(q)) },
  { name: 'deployment-default', env: { ...BASE_ENV, INFERENCE_MEMORY_BUDGET_GIB: '5', LLAMACPP_AUTOCONFIG_CACHE_RAM_MAX_MIB: '256' }, budgetSetting: null, requests: SMALL },
  { name: 'string-setting', env: { ...BASE_ENV, INFERENCE_MEMORY_BUDGET_GIB: '5' }, budgetSetting: '{"budgetGib":" 7 "}', requests: both([q('good')]) },
  { name: 'unreadable-setting', env: { ...BASE_ENV, INFERENCE_MEMORY_BUDGET_GIB: 'lots' }, budgetSetting: 'not json', requests: both([q('good')]) },
  { name: 'array-setting', env: { ...BASE_ENV, INFERENCE_MEMORY_BUDGET_GIB: '6' }, budgetSetting: '{"budgetGib":[3]}', requests: both([q('good')]) },
  // The boot-time memory-limit fault cannot show while an inference budget exists (at least 2 GiB).
  { name: 'memory-limit', env: { ...BASE_ENV, LLAMACPP_MEMORY_LIMIT: 'not a size' }, budgetSetting: '{"budgetGib":6}', requests: both([q('good')]) },
  { name: 'duplicate-sections', env: BASE_ENV, budgetSetting: '{"budgetGib":6}', ini: '[a]\nmodel = /models/good.gguf\n[a]\nc = 1\n', requests: both([q('a'), q('b')]) },
  { name: 'ini-missing', env: BASE_ENV, budgetSetting: '{"budgetGib":6}', ini: null, requests: both([q('good')]) },
  { name: 'ini-symlink', env: BASE_ENV, budgetSetting: '{"budgetGib":6}', iniSymlink: true, requests: both([q('good')]) },
  { name: 'ini-too-large', env: BASE_ENV, budgetSetting: '{"budgetGib":6}', iniPad: 1024 * 1024, requests: both([q('good')]) },
  { name: 'ini-bom-invalid-utf8', env: BASE_ENV, budgetSetting: '{"budgetGib":6}', iniB64: b64(Buffer.concat([Buffer.from('﻿[good]\nmodel = /models/good.gguf\n; '), Buffer.from([0xff, 0xfe, 0xc3]), Buffer.from('\n[\xe9t\xe9]\nmodel = /models/hybrid.gguf\n', 'latin1'), Buffer.from('[good2]\nmodel = /models/good.gguf\nc= 8192\n')])), requests: both(['good', '﻿[good]', 'good2', '�t�'].map(q)) },
  { name: 'no-models-path', env: { LLAMACPP_PRESET_PATH: '$ROOT/models.ini' }, budgetSetting: '{"budgetGib":6}', requests: both([q('good'), '']) },
  { name: 'no-preset-path', env: { LLAMACPP_MODELS_PATH: '$ROOT/models' }, budgetSetting: '{"budgetGib":6}', requests: both([q('good'), '']) },
  { name: 'no-cache-path', env: { LLAMACPP_PRESET_PATH: '$ROOT/models.ini', LLAMACPP_MODELS_PATH: '$ROOT/models' }, budgetSetting: '{"budgetGib":6}', requests: both([q('cached'), q('good')]) },
  { name: 'router-down', env: BASE_ENV, budgetSetting: '{"budgetGib":6}', listing: 'down', requests: both(['good', 'routed', 'weird', 'vision'].map(q)) },
  { name: 'router-error-body', env: BASE_ENV, budgetSetting: '{"budgetGib":6}', listing: { status: 500, text: JSON.stringify({ data: [{ id: 'good', status: { args: ['--model', '/models/hybrid.gguf'] } }] }) }, requests: both(['good'].map(q)) },
  { name: 'router-null-row', env: BASE_ENV, budgetSetting: '{"budgetGib":6}', listing: { status: 200, text: JSON.stringify({ data: [null, { id: 'good', status: { args: ['--model', '/models/hybrid.gguf'] } }] }) }, requests: both(['good', 'routed'].map(q)) },
  { name: 'router-not-json', env: BASE_ENV, budgetSetting: '{"budgetGib":6}', listing: { status: 200, text: 'plain text' }, requests: both(['good'].map(q)) },
  { name: 'router-data-object', env: BASE_ENV, budgetSetting: '{"budgetGib":6}', listing: { status: 200, text: '{"data":{"find":1}}' }, requests: both(['good'].map(q)) },
  { name: 'router-args-object', env: BASE_ENV, budgetSetting: '{"budgetGib":6}', listing: { status: 200, text: '{"data":[{"id":"good","status":{"args":{"0":"--model"}}},{"id":"zero","status":{"args":0}}]}' }, requests: both(['good', 'zero'].map(q)) },
];

function envOf(s, root) {
  const env = {};
  for (const [k, v] of Object.entries(s.env)) env[k] = v.replace('$ROOT', root);
  return env;
}
function lay(root, s) {
  for (const f of MODEL_FILES) {
    const file = path.join(root, f.path);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    if (f.symlink) { fs.symlinkSync(f.symlink, file); continue; }
    fs.writeFileSync(file, Buffer.from(f.b64, 'base64'));
    fs.truncateSync(file, f.size);
  }
  const ini = path.join(root, 'models.ini');
  if (s.iniSymlink) { fs.writeFileSync(path.join(root, 'real.ini'), INI); fs.symlinkSync('real.ini', ini); }
  else if (s.iniB64) fs.writeFileSync(ini, Buffer.from(s.iniB64, 'base64'));
  // Over the 1 MiB editor limit: one section, then a comment of `iniPad` bytes.
  else if (s.iniPad) fs.writeFileSync(ini, '[good]\nmodel = /models/good.gguf\n; ' + 'x'.repeat(s.iniPad) + '\n');
  else if (s.ini !== null) fs.writeFileSync(ini, s.ini === undefined ? INI : s.ini);
}
async function router(listing) {
  const seen = [];
  const server = http.createServer((rq, rs) => {
    seen.push(rq.method + ' ' + rq.url);
    if (rq.method === 'GET' && rq.url === '/models') {
      rs.writeHead(listing.status, { 'Content-Type': 'application/json' });
      return rs.end(listing.text);
    }
    rs.writeHead(404); rs.end('{}');
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  return { server, seen, port: server.address().port };
}
// index.cjs's autoconfig and inference budget wiring, from `env`.
function manager(env, budgetSetting, baseUrl) {
  const explicit = Number(env.LLAMACPP_AUTOCONFIG_MEMORY_GIB);
  let autoconfigBudget;
  if (explicit) autoconfigBudget = { budgetGib: explicit };
  else {
    try { autoconfigBudget = { budgetGib: req('llamacpp-autoconfig.cjs').parseMemoryLimit(env.LLAMACPP_MEMORY_LIMIT) || 0 }; }
    catch (err) { autoconfigBudget = { budgetGib: 0, budgetFault: `the container memory limit (LLAMACPP_MEMORY_LIMIT) could not be evaluated at startup: ${err?.message || err}` }; }
  }
  const inferenceBudget = createInferenceBudget({ store: { get: (k) => (k === 'inference:memory-budget' ? budgetSetting : undefined) }, env });
  return createModelManager({
    kind: 'llamacpp', presetPath: env.LLAMACPP_PRESET_PATH, baseUrl, apiKey: 'local', fetchJson, inferenceBudget,
    autoconfig: {
      modelsPath: env.LLAMACPP_MODELS_PATH || '', budgetGib: autoconfigBudget.budgetGib, ...(autoconfigBudget.budgetFault ? { budgetFault: autoconfigBudget.budgetFault } : {}),
      explicitBudgetGib: Number(env.LLAMACPP_AUTOCONFIG_MEMORY_GIB) || 0, cacheRamMaxMib: cacheRamLimits(env).capMib, cacheRam: cacheRamLimits(env),
      cachePath: env.LLAMACPP_CACHE_PATH || '', memoryFloorGib: 2,
    },
  });
}
async function serveRoutes(modelManager, env) {
  const routes = createModelRoutes({
    json, fetchJson, env, modelManager, readBody: async () => '', readJson: async () => ({}),
    getProvider: () => ({ baseUrl: 'http://127.0.0.1:9', apiKey: 'local' }), providerHeaders: () => ({}), DEFAULT_PROVIDER_ID: 'default',
    createVisionProbe: () => async () => ({ supported: false }), missingRoles: () => null, currentWorkspace: () => ({ userId: 'u' }),
    service: { modelScanCache: new Map(), refreshModelScan() {}, autoRoles: () => null, setAutoRoles() {}, ensureRolesLoaded: async () => {}, servedCatalogue: async () => [], modelsInstalled: async () => [], deriveUserModelName: (x) => x, lastLoadedModel: () => null, clearLastLoadedModel() {}, clearRoleReferences: () => [] },
  });
  const server = http.createServer(async (rq, rs) => {
    const url = new URL(rq.url, 'http://h');
    try {
      const handled = await routes(rq, rs, { path: url.pathname, authn: { user: { id: 'u', role: rq.headers['x-role'] } }, url });
      if (!handled) { rs.writeHead(599); rs.end('unhandled'); }
    } catch (err) { const f = errorResponse(err); json(rs, f.status, f.body); }
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  return server;
}
async function main() {
  const out = { recordedWith: 'noevia-core ' + (process.env.CORE_SHA || 'unknown'), files: MODEL_FILES, ini: INI, listing: JSON.stringify(LISTING), scenarios: [] };
  for (const s of scenarios) {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'model-sizing-'));
    try {
      lay(root, s);
      const listing = s.listing === undefined ? { status: 200, text: out.listing } : s.listing;
      const engine = listing === 'down' ? null : await router(listing);
      const env = envOf(s, root);
      const mm = manager(env, s.budgetSetting, engine ? `http://127.0.0.1:${engine.port}/v1` : 'http://127.0.0.1:9/v1');
      const web = await serveRoutes(mm, env);
      const results = [];
      for (const r of s.requests) {
        const res = await fetch(`http://127.0.0.1:${web.address().port}/api/models/${r.route}${r.query}`, { headers: { 'x-role': r.role || 'admin' } });
        results.push({ ...r, role: r.role || 'admin', status: res.status, cacheControl: res.headers.get('cache-control'), body: await res.text() });
      }
      web.close(); engine?.server.close();
      if (engine && engine.seen.some((x) => x !== 'GET /models')) throw Error('the reads asked the router for more than its listing: ' + engine.seen.join(', '));
      const { requests, ...rest } = s;
      out.scenarios.push({ ...rest, listing: s.listing === undefined ? undefined : s.listing, results });
    } finally { fs.rmSync(root, { recursive: true, force: true }); }
  }
  process.stdout.write(JSON.stringify(out, null, 1) + '\n');
}
main().then(() => process.exit(0), (e) => { console.error(e); process.exit(1); });
