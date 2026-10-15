#!/usr/bin/env node
'use strict';
// Test-oracle glue only (sbstndalton/noevia#1650): core's actual GET /api/stats handler
// (server/routes/models.cjs createModelRoutes) over core's actual model-manager adapters
// (model-manager.cjs, llamacpp-manager.cjs) and http.cjs fetchJson, against a synthetic engine on
// loopback that serves each case's canned replies. Records the reply Node sends (status, exact
// body bytes; a throw is index.cjs's catch, http.cjs errorResponse) and every engine request.
// Synthetic data only; nothing leaves 127.0.0.1.
// Usage: node oracle.cjs <noevia-core checkout> <output.json>
const fs = require('node:fs'), path = require('node:path'), http = require('node:http');
const cp = require('node:child_process'), crypto = require('node:crypto');
const core = path.resolve(process.argv[2] || ''), out = process.argv[3];
if (!out) throw Error('usage: oracle.cjs <noevia-core checkout> <output.json>');
const sha = cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
if (cp.execFileSync('git', ['-C', core, 'status', '--porcelain', '--untracked-files=no'], { encoding: 'utf8' }).trim())
  throw Error('oracle checkout must be clean');
const SOURCES = ['server/routes/models.cjs', 'server/model-manager.cjs', 'server/llamacpp-manager.cjs', 'server/llamacpp-metrics.cjs',
  'server/mtp.cjs', 'server/engine-stats.cjs', 'server/short-read.cjs', 'server/http.cjs'];
const { createModelRoutes } = require(path.join(core, 'server/routes/models.cjs'));
const { createModelManager } = require(path.join(core, 'server/model-manager.cjs'));
const { fetchJson, json, errorResponse } = require(path.join(core, 'server/http.cjs'));
const { reportedTokenRate } = require(path.join(core, 'server/engine-stats.cjs'));
const USER = 'aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa';
// listModels() (a read the handler never uses for the llama.cpp kind) warns that the long-profile
// pairing module is not built in a bare checkout; nothing else is silenced.
const warn = console.warn;
console.warn = (...a) => { if (!String(a[0]).startsWith('[long-profile]')) warn(...a); };
const OVERSIZE = 8 * 1024 * 1024 + 1;

// ── The synthetic engine: `routes` maps a request target (path and query) to a reply.
// A reply is { status, headers?, text | hex | oversize: true | close: true }.
let routes = {}, seen = [];
const engine = http.createServer((req, res) => {
  seen.push({ path: req.url, authorization: req.headers.authorization ?? null, contentType: req.headers['content-type'] ?? null });
  const r = routes[req.url];
  if (!r) { res.writeHead(404, { 'Content-Type': 'text/plain' }); return res.end('not found'); }
  if (r.close) return req.socket.destroy();
  res.writeHead(r.status, r.headers || {});
  if (r.oversize) return res.end(Buffer.alloc(OVERSIZE, 0x20));
  res.end(r.hex !== undefined ? Buffer.from(r.hex, 'hex') : (r.text ?? ''));
});

async function run(c, port) {
  routes = c.routes; seen = [];
  const modelManager = createModelManager({ kind: c.kind, baseUrl: `http://127.0.0.1:${port}${c.base}`, apiKey: c.apiKey, fetchJson });
  const handle = createModelRoutes({
    json, readBody: async () => '', readJson: async () => ({}), fetchJson, env: {}, modelManager,
    getProvider: () => null, providerHeaders: () => ({}), DEFAULT_PROVIDER_ID: 'default', createVisionProbe: () => async () => ({}),
    reportedTokenRate, missingRoles: () => [], currentWorkspace: () => ({ userId: USER }),
    service: { invalidateModelReads() {} },
  });
  const res = { status: null, body: null, writeHead(s) { this.status = s; }, setHeader() {}, end(b) { this.body = String(b); } };
  try {
    const handled = await handle({ method: 'GET' }, res, { path: '/api/stats', authn: { user: { id: USER, role: 'member' } }, url: new URL('http://localhost/api/stats') });
    if (!handled) throw Error(`${c.name}: not handled`);
  } catch (err) {
    const failure = errorResponse(err);
    json(res, failure.status, failure.body);
  }
  return { status: res.status, body: res.body, requests: seen.slice().sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0)) };
}

// ── Cases. `text` replies are UTF-8; `hex` is raw bytes.
const listing = (data) => ({ status: 200, text: JSON.stringify({ data }) });
const metrics = (text) => ({ status: 200, text });
const mq = (id) => `/metrics?model=${encodeURIComponent(id)}&autoload=false`;
const loaded = (id, extra = {}) => ({ id, status: { value: 'loaded' }, ...extra });
const full = 'llamacpp:prompt_tokens_total 120\nllamacpp:tokens_predicted_total 80\nllamacpp:predicted_tokens_seconds 12.5\nllamacpp:spec_decode_num_draft_tokens_total 40\nllamacpp:spec_decode_num_accepted_tokens_total 25\n';
const L = (name, routes, more = {}) => ({ name, kind: 'llamacpp', base: '/v1', apiKey: '', routes, ...more });
const cases = [
  L('llamacpp-two-loaded', { '/models': listing([loaded('chat-a'), loaded('chat-b'), { id: 'idle', status: { value: 'unloaded' } }]), [mq('chat-a')]: metrics(full), [mq('chat-b')]: metrics('llamacpp:prompt_tokens_total 3\nllamacpp:tokens_predicted_total 0.5\nllamacpp:predicted_tokens_seconds 1e-7\n') }),
  // bins/noevia-server tests/stats.rs serves exactly this engine through the front.
  L('llamacpp-front-test', { '/models': listing([loaded('chat-a'), { id: 'idle', status: { value: 'unloaded' } }]), [mq('chat-a')]: metrics(full) }),
  L('llamacpp-none-loaded', { '/models': listing([{ id: 'idle', status: { value: 'unloaded' } }]) }),
  L('llamacpp-engine-down', {}),
  L('llamacpp-listing-500', { '/models': { status: 500, text: '{"data":[]}' } }),
  L('llamacpp-listing-not-json', { '/models': { status: 200, text: 'hello' } }),
  L('llamacpp-listing-bom', { '/models': { status: 200, hex: 'efbbbf' + Buffer.from(JSON.stringify({ data: [loaded('a')] })).toString('hex') } }),
  L('llamacpp-listing-null', { '/models': { status: 200, text: 'null' } }),
  L('llamacpp-data-object', { '/models': { status: 200, text: '{"data":{"0":1}}' } }),
  L('llamacpp-data-string', { '/models': { status: 200, text: '{"data":"x"}' } }),
  L('llamacpp-data-zero', { '/models': { status: 200, text: '{"data":0}' } }),
  L('llamacpp-null-model', { '/models': { status: 200, text: '{"data":[{"id":"a","status":{"value":"loaded"}},null]}' }, [mq('a')]: metrics(full) }),
  L('llamacpp-primitive-models', { '/models': { status: 200, text: '{"data":[1,"x",true,[],{"status":null},{"status":"loaded"}]}' } }),
  L('llamacpp-redirect', { '/models': { status: 302, headers: { Location: '/elsewhere' }, text: '' } }),
  L('llamacpp-redirect-no-location', { '/models': { status: 307, text: '{"data":[]}' } }),
  L('llamacpp-300-not-redirect', { '/models': { status: 300, text: '{"data":[]}' } }),
  L('llamacpp-closed', { '/models': { close: true } }),
  L('llamacpp-oversize-listing', { '/models': { status: 200, oversize: true } }),
  L('llamacpp-oversize-metrics', { '/models': listing([loaded('a')]), [mq('a')]: { status: 200, oversize: true } }),
  L('llamacpp-metrics-404', { '/models': listing([loaded('a'), loaded('b')]), [mq('b')]: metrics(full) }),
  L('llamacpp-metrics-closed', { '/models': listing([loaded('a'), loaded('b')]), [mq('a')]: { close: true }, [mq('b')]: metrics(full) }),
  L('llamacpp-metrics-json-number', { '/models': listing([loaded('a')]), [mq('a')]: { status: 200, text: '123' } }),
  L('llamacpp-metrics-json-tostring-throws', { '/models': listing([loaded('a'), loaded('b')]), [mq('a')]: { status: 200, text: '{"toString":"x"}' }, [mq('b')]: metrics(full) }),
  L('llamacpp-metrics-json-array', { '/models': listing([loaded('a')]), [mq('a')]: { status: 200, text: '["llamacpp:prompt_tokens_total 5","x"]' } }),
  L('llamacpp-metrics-crlf', { '/models': listing([loaded('a')]), [mq('a')]: metrics(full.replace(/\n/g, '\r\n')) }),
  L('llamacpp-metrics-odd-values', { '/models': listing([loaded('a')]), [mq('a')]: metrics('llamacpp:prompt_tokens_total 0x10\nllamacpp:tokens_predicted_total Infinity\nllamacpp:predicted_tokens_seconds -0\nllamacpp:spec_decode_num_draft_tokens_total 0b1010\nllamacpp:spec_decode_num_accepted_tokens_total 0o7\n') }),
  L('llamacpp-metrics-unicode-space', { '/models': listing([loaded('a')]), [mq('a')]: metrics('llamacpp:prompt_tokens_total 7\nllamacpp:tokens_predicted_total 9\nllamacpp:predicted_tokens_seconds﻿2.5\nllamacpp:spec_decode_num_draft_tokens_total\u00852\n') }),
  L('llamacpp-mtp-rejected-rows', { '/models': listing([loaded('a'), loaded('b'), loaded('c')]), [mq('a')]: metrics('llamacpp:spec_decode_num_draft_tokens_total 0\nllamacpp:spec_decode_num_accepted_tokens_total 0\n'), [mq('b')]: metrics('llamacpp:spec_decode_num_draft_tokens_total 5\nllamacpp:spec_decode_num_accepted_tokens_total 6\n'), [mq('c')]: metrics('llamacpp:spec_decode_num_draft_tokens_total 3\n') }),
  L('llamacpp-five-loaded-batches', { '/models': listing(['m1', 'm2', 'm3', 'm4', 'm5'].map((id) => loaded(id))), ...Object.fromEntries(['m1', 'm2', 'm3', 'm4', 'm5'].map((id, i) => [mq(id), metrics(`llamacpp:prompt_tokens_total ${i + 1}\nllamacpp:tokens_predicted_total ${0.1 * (i + 1)}\nllamacpp:predicted_tokens_seconds ${1 / 3}\n`)])) }),
  L('llamacpp-odd-ids', { '/models': listing([loaded('ü ö/€&?=#'), loaded('😀'), loaded(42), loaded(null), { status: { value: 'loaded' } }, loaded({ a: 1 }), loaded([1, [2, null]]), loaded(1e21), loaded(-0)]),
    [mq('ü ö/€&?=#')]: metrics(full), [mq('😀')]: metrics(full), [mq(42)]: metrics(full), [mq(null)]: metrics(full), [mq(undefined)]: metrics(full), [mq({ a: 1 })]: metrics(full), [mq([1, [2, null]])]: metrics(full), [mq(1e21)]: metrics(full), [mq(-0)]: metrics(full) }),
  L('llamacpp-lone-surrogate-id', { '/models': { status: 200, text: '{"data":[{"id":"x\\ud800","status":{"value":"loaded"}},{"id":"y","status":{"value":"loaded"}}]}' }, [mq('y')]: metrics(full) }),
  L('llamacpp-tostring-id', { '/models': { status: 200, text: '{"data":[{"id":{"toString":1},"status":{"value":"loaded"}}]}' } }),
  L('llamacpp-id-escapes', { '/models': { status: 200, text: '{"data":[{"id":"q\\"\\\\\\n\\t\\u0001\\u2028<>","status":{"value":"loaded"}}]}' }, [mq('q"\\\n\t\u0001 <>')]: metrics(full) }),
  L('llamacpp-duplicate-keys-and-index-order', { '/models': { status: 200, text: '{"data":[{"id":"first","status":{"value":"unloaded"},"status":{"value":"loaded"},"id":"a"},{"id":{"b":1,"2":2,"1":1},"status":{"value":"loaded"}}]}' }, [mq('a')]: metrics(full), [mq('[object Object]')]: metrics(full) }),
  L('llamacpp-big-numbers', { '/models': { status: 200, text: '{"data":[{"id":"a","status":{"value":"loaded"},"meta":{"size":1e999}}]}' }, [mq('a')]: metrics('llamacpp:prompt_tokens_total 1.7976931348623157e308\nllamacpp:tokens_predicted_total 123456789012345678901234567890\nllamacpp:predicted_tokens_seconds 0.000001\n') }),
  L('llamacpp-zero-counters', { '/models': listing([loaded('a'), loaded('b')]), [mq('a')]: metrics('llamacpp:prompt_tokens_total 0\nllamacpp:tokens_predicted_total -0\nllamacpp:spec_decode_num_draft_tokens_total 4\nllamacpp:spec_decode_num_accepted_tokens_total 0\n'), [mq('b')]: metrics('llamacpp:prompt_tokens_total 0.0\nllamacpp:tokens_predicted_total 0x0\nllamacpp:spec_decode_num_draft_tokens_total 2\nllamacpp:spec_decode_num_accepted_tokens_total -0\n') }),
  L('llamacpp-sum-overflows', { '/models': listing([loaded('a'), loaded('b')]), [mq('a')]: metrics('llamacpp:prompt_tokens_total 1.7976931348623157e308\n'), [mq('b')]: metrics('llamacpp:prompt_tokens_total 1.7976931348623157e308\n') }),
  L('llamacpp-invalid-utf8', { '/models': { status: 200, hex: Buffer.from('{"data":[{"id":"aÿ').toString('hex').replace('c3bf', 'ff') + Buffer.from('","status":{"value":"loaded"}}]}').toString('hex') }, [mq('a�')]: metrics(full) }),
  L('llamacpp-base-plain', { '/models': listing([loaded('a')]), [mq('a')]: metrics(full) }, { base: '' }),
  L('llamacpp-base-slashes', { '/models': listing([loaded('a')]), [mq('a')]: metrics(full) }, { base: '/v1///' }),
  L('llamacpp-base-prefix', { '/p/models': listing([loaded('a')]), [`/p${mq('a')}`]: metrics(full) }, { base: '/p/' }),
  L('llamacpp-api-key', { '/models': listing([loaded('a')]), [mq('a')]: metrics(full) }, { apiKey: 'synthetic-key ' }),
  L('llamacpp-api-key-local', { '/models': listing([]) }, { apiKey: 'local' }),
  { name: 'none-kind', kind: 'none', base: '', apiKey: '', routes: {} },
];
const LEM = (name, routes, more = {}) => ({ name, kind: 'lemonade', base: '', apiKey: '', routes, ...more });
const lemStats = { status: 200, text: JSON.stringify({ scope: 'engine', tokens_per_second: 13.18504153409047, output_tokens: 109, time_to_first_token: 0.25, input_tokens: 12, input_tokens_total: 1e21, output_tokens_total: 5e-7, request_count_total: 3 }) };
const lemSys = { status: 200, text: JSON.stringify({ cpu_percent: 12, gpu_percent: 33.3, vram_gb: 7.5, memory_gb: 1e-7 }) };
const lemHealth = (models) => ({ status: 200, text: JSON.stringify({ all_models_loaded: models }) });
const lemModels = (data) => ({ status: 200, text: JSON.stringify({ data }) });
const mtpModel = (name, args = '--spec-type draft-mtp', extra = {}) => ({ model_name: name, loaded: true, type: 'llm', recipe_options: { llamacpp_args: args }, ...extra });
const lemMetrics = (text) => ({ status: 200, text });
const counters = 'lemonade_llamacpp_spec_decode_num_draft_tokens_total{model_name="m"} 10\nlemonade_llamacpp_spec_decode_num_accepted_tokens_total{model_name="m"} 4\nlemonade_llamacpp_spec_decode_num_draft_tokens_total{x="1",model_name="m"} 6\n';
cases.push(
  LEM('lemonade-full', { '/v1/stats': lemStats, '/v1/system-stats': lemSys, '/api/v1/health': lemHealth([mtpModel('m'), mtpModel('n', '--spec-type none'), mtpModel('o', ''), { model_name: 'p', loaded: false, type: 'llm' }]), '/metrics': lemMetrics(counters), '/api/v1/models': lemModels([{ id: 'o', recipe: 'llamacpp', labels: ['mtp'] }]) }),
  LEM('lemonade-down', {}),
  LEM('lemonade-stats-null', { '/v1/stats': { status: 200, text: 'null' } }),
  LEM('lemonade-system-null', { '/v1/stats': lemStats, '/v1/system-stats': { status: 200, text: 'null' } }),
  LEM('lemonade-stats-text', { '/v1/stats': { status: 200, text: 'up' }, '/v1/system-stats': { status: 200, text: '[1]' } }),
  LEM('lemonade-scope-values', { '/v1/stats': { status: 200, text: '{"scope":{"b":[1,"x"],"1":true},"tokens_per_second":1e400,"output_tokens":5,"input_tokens":1e999}' } }),
  LEM('lemonade-health-null', { '/v1/stats': lemStats, '/api/v1/health': { status: 200, text: 'null' } }),
  LEM('lemonade-models-null-body', { '/api/v1/health': lemHealth([mtpModel('m')]), '/api/v1/models': { status: 200, text: 'null' } }),
  LEM('lemonade-health-not-iterable', { '/api/v1/health': { status: 200, text: '{"all_models_loaded":{"a":1}}' } }),
  LEM('lemonade-health-string', { '/api/v1/health': { status: 200, text: '{"all_models_loaded":"ab😀"}' }, '/api/v1/models': lemModels([{ id: 'x', recipe: 'llamacpp', labels: 'xmtpx' }]) }),
  LEM('lemonade-installed-null-row', { '/api/v1/health': lemHealth([mtpModel('m', undefined)]), '/api/v1/models': lemModels([null]) }),
  LEM('lemonade-installed-not-array', { '/api/v1/health': lemHealth([mtpModel('m')]), '/api/v1/models': { status: 200, text: '{"data":"x"}' } }),
  LEM('lemonade-registry-capability', { '/api/v1/health': lemHealth([{ model_name: 'r1', loaded: true, type: 'llm' }, { model_name: 'r2', loaded: 1, type: 'llm' }, { model_name: 'r3', loaded: true, type: 'llm' }]), '/metrics': lemMetrics('lemonade_llamacpp_spec_decode_num_draft_tokens_total{model_name="r1"} 8\nlemonade_llamacpp_spec_decode_num_accepted_tokens_total{model_name="r1"} 8\n'),
    '/api/v1/models': lemModels([{ id: 'r1', recipe: 'llamacpp', labels: ['mtp'] }, { model_name: 'r2', recipe: 'llamacpp', labels: 'has-mtp', recipe_options: { llamacpp_args: '--spec-type=draft-mtp,other' } }, { id: 'r3', recipe: 'llamacpp', labels: ['MTP'] }]) }),
  LEM('lemonade-labels-throw', { '/api/v1/health': lemHealth([{ model_name: 'r', loaded: true, type: 'llm' }]), '/api/v1/models': lemModels([{ id: 'r', recipe: 'llamacpp', labels: 5 }]) }),
  LEM('lemonade-args-tostring-throws', { '/api/v1/health': lemHealth([{ model_name: 'r', loaded: true, type: 'llm', recipe_options: { llamacpp_args: { toString: 1 } } }]) }),
  LEM('lemonade-label-json-throws', { '/api/v1/health': lemHealth([mtpModel('m')]), '/metrics': lemMetrics('lemonade_llamacpp_spec_decode_num_draft_tokens_total{model_name="\\x"} 1\n') }),
  LEM('lemonade-label-raw-cr-throws', { '/api/v1/health': lemHealth([mtpModel('m')]), '/metrics': lemMetrics('lemonade_llamacpp_spec_decode_num_draft_tokens_total{model_name="a\rb"} 1\n') }),
  LEM('lemonade-labels-edge', { '/api/v1/health': lemHealth([mtpModel('m'), mtpModel('é"q'), mtpModel('n')]), '/metrics': lemMetrics('lemonade_llamacpp_spec_decode_num_draft_tokens_total{xmodel_name="n",model_name="m"} 7 trailing\nlemonade_llamacpp_spec_decode_num_accepted_tokens_total{model_name="m",model_name="n"} 3\nlemonade_llamacpp_spec_decode_num_draft_tokens_total{model_name="\\u00e9\\"q"}\t2\nlemonade_llamacpp_spec_decode_num_accepted_tokens_total{model_name="\\u00e9\\"q"} 0x1\nlemonade_llamacpp_spec_decode_num_draft_tokens_total{model_name="n"} -1\nlemonade_llamacpp_spec_decode_num_draft_tokens_total{model_name="n"}5\nlemonade_llamacpp_spec_decode_num_draft_tokens_total {model_name="n"} 5\n') }),
  LEM('lemonade-model-name-tostring-throws', { '/api/v1/health': lemHealth([mtpModel({ toString: 1 })]) }),
  LEM('lemonade-metrics-json', { '/api/v1/health': lemHealth([mtpModel('m')]), '/metrics': { status: 200, text: '{"toString":2}' } }),
  LEM('lemonade-api-key', { '/v1/stats': lemStats }, { apiKey: 'lemon-key' }),
);

// ── A seeded random batch (mulberry32), so the fixture is reproducible.
let seed = 0x5eed1650;
const rand = () => { seed |= 0; seed = (seed + 0x6d2b79f5) | 0; let t = Math.imul(seed ^ (seed >>> 15), 1 | seed); t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t; return ((t ^ (t >>> 14)) >>> 0) / 4294967296; };
const pick = (a) => a[Math.floor(rand() * a.length)];
const NAMES = ['predicted_tokens_seconds', 'prompt_tokens_total', 'tokens_predicted_total', 'spec_decode_num_draft_tokens_total', 'spec_decode_num_accepted_tokens_total', 'other_total', 'Bad', 'a1'];
const SEPS = [' ', ' ', ' ', '\t', '  ', ' ', ' ', '', '　', '\u0085', '﻿'];
const VALUES = () => pick([String(Math.floor(rand() * 1000)), String(rand() * 100), String(-rand()), '0x' + Math.floor(rand() * 4096).toString(16), '1e400', 'Infinity', '-Infinity', 'NaN', '1e', '.5', '+3', '0b11', '1 2', '1e-320', '00012', '1_0', String(2 ** 53 + 1), '0', '-0', '0.0', '2', '3']);
const metricText = () => {
  const lines = [];
  for (let i = Math.floor(rand() * 9); i > 0; i--) lines.push(pick([`llamacpp:${pick(NAMES)}${pick(SEPS)}${VALUES()}`, '# HELP x', '', `llamacpp:${pick(NAMES)}${pick(SEPS)}${VALUES()}${pick(['', ' ', '\r'])}`]));
  return lines.join(pick(['\n', '\n', '\r\n']));
};
const IDS = ['a', 'b', 'c', 'ü', '😀', 7, null, 'x y', 'a'];
for (let n = 0; n < 160; n++) {
  const models = [];
  for (let i = Math.floor(rand() * 7); i > 0; i--) models.push(pick([() => loaded(pick(IDS)), () => loaded(pick(IDS)), () => ({ id: pick(IDS), status: { value: pick(['unloaded', 'loading', 'Loaded']) } }), () => ({ id: pick(IDS) }), () => ({ id: pick(IDS), status: null }), () => pick([1, 'x', [], true])])());
  const r = { '/models': rand() < 0.08 ? pick([{ status: 503, text: '{}' }, { close: true }, { status: 200, text: '{"data":null}' }]) : listing(models) };
  for (const id of IDS) if (rand() < 0.85) r[mq(id)] = rand() < 0.1 ? pick([{ status: 500, text: 'x' }, { close: true }, { status: 200, text: '7' }]) : metrics(metricText());
  cases.push(L(`random-llamacpp-${n}`, r));
}
const LNAMES = ['m', 'n', 'é', 'x"y'];
const lemLine = () => {
  const kind = pick(['draft', 'accepted', 'draft', 'accepted', 'drafted']);
  const name = pick(LNAMES);
  const label = pick([`model_name=${JSON.stringify(name)}`, `a="1",model_name=${JSON.stringify(name)}`, `amodel_name=${JSON.stringify(name)}`, `model_name=${JSON.stringify(name)},model_name="n"`, 'model_name="unterminated']);
  return `lemonade_llamacpp_spec_decode_num_${kind}_tokens_total{${label}}${pick(SEPS)}${VALUES()}${pick(['', ' tail', '\r'])}`;
};
for (let n = 0; n < 80; n++) {
  const health = [];
  for (let i = Math.floor(rand() * 5); i > 0; i--) health.push({ model_name: pick(LNAMES), loaded: pick([true, true, false, 1, 0, 'yes']), type: pick(['llm', 'llm', 'embedding']), ...(rand() < 0.7 ? { recipe_options: { llamacpp_args: pick(['--spec-type draft-mtp', '--spec-type none', '--spec-type=draft-mtp,x', '"--spec-type" draft-mtp', '--spec-type', '-c 4096', '']) } } : {}) });
  const installed = [];
  for (let i = Math.floor(rand() * 4); i > 0; i--) installed.push({ [pick(['id', 'model_name'])]: pick(LNAMES), recipe: pick(['llamacpp', 'other']), labels: pick([['mtp'], ['vision'], 'mtp', null]), ...(rand() < 0.5 ? { recipe_options: { llamacpp_args: pick(['--spec-type draft-mtp', '--spec-type none', '']) } } : {}) });
  const lines = []; for (let i = Math.floor(rand() * 10); i > 0; i--) lines.push(lemLine());
  const r = { '/api/v1/health': lemHealth(health), '/api/v1/models': lemModels(installed), '/metrics': lemMetrics(lines.join('\n')) };
  if (rand() < 0.5) r['/v1/stats'] = { status: 200, text: JSON.stringify({ scope: pick(['engine', '', 0, null]), tokens_per_second: rand() * 50, output_tokens: Math.floor(rand() * 200), time_to_first_token: pick([rand(), '1', null]) }) };
  if (rand() < 0.5) r['/v1/system-stats'] = { status: 200, text: JSON.stringify({ cpu_percent: rand() * 100, gpu_percent: pick([null, 5, '5']), vram_gb: rand() * 16 }) };
  cases.push(LEM(`random-lemonade-${n}`, r));
}

(async () => {
  await new Promise((resolve) => engine.listen(0, '127.0.0.1', resolve));
  const port = engine.address().port;
  const results = [];
  for (const c of cases) {
    const got = await run(c, port);
    results.push({ ...c, expected: { status: got.status, body: got.body }, requests: got.requests });
  }
  engine.close();
  const sources = Object.fromEntries(SOURCES.map((f) => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(core, f))).digest('hex')]));
  fs.writeFileSync(out, JSON.stringify({ core: sha, sources, cases: results }, null, 1) + '\n');
  process.stdout.write(`stats fixtures=${results.length} core=${sha}\n`);
  process.exit(0);
})().catch((e) => { console.error(e); process.exit(1); });
