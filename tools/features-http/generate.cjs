'use strict';
// Original-Node module HTTP/write oracle only. No Rust import, network, model or real account data.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');
const CORE_REF = '6e42bc6d4bfe6756db569fd008511f8812b12a9a';
const core = path.resolve(process.argv[2] || '');
const output = path.resolve(__dirname, '../../crates/server-features/tests/fixtures/core-feature-http-v1.json');
if (execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim() !== CORE_REF) throw Error('Wrong original Core checkout');
const files = ['server/features.cjs', 'server/routes/features.cjs', 'server/http.cjs', 'server/index.cjs', 'server/device-auth.cjs', 'server/feature-catalog.cjs', 'server/feature-catalog.json', 'server/feature-catalog.lock.json'];
const sha = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
const source = Object.fromEntries(files.map(file => {
  const bytes = fs.readFileSync(path.join(core, file));
  const committed = execFileSync('git', ['-C', core, 'show', `${CORE_REF}:${file}`], { maxBuffer: 2 * 1024 * 1024 });
  if (!bytes.equals(committed)) throw Error(`Original source differs from frozen commit: ${file}`);
  return [file, sha(bytes)];
}));
const original = require(path.join(core, 'server/features.cjs'));
const { createFeatureRoutes } = require(path.join(core, 'server/routes/features.cjs'));
const http = require(path.join(core, 'server/http.cjs'));
const names = Object.keys(original.REGISTRY);
const unavailableReasons = {
  stepSupervision: 'Set up the decision service below before enabling this experiment.',
  toolGate: 'Set up the decision service below before enabling this experiment.',
  chatFraming: 'Set up the decision service below before enabling this experiment.',
  framingReasoner: 'Set up the decision service below before enabling this experiment.',
  systemOneRouting: 'Not configured. The operator needs to point it at a dedicated local decision-model endpoint, then restart noevia.',
  browserExecutor: 'Needs Playwright installed on this server; it is not in this image.',
  codePipeline: 'Needs Code mode running in the code sandbox (CODE_HARNESS_ENDPOINT).',
  codeMerge: 'Needs Code mode running in the code sandbox (CODE_HARNESS_ENDPOINT).',
  nativeClientAuth: 'Needs TRUST_PROXY on so sign-in limits can tell clients apart.',
};
const specs = [];
const add = (id, value = {}) => specs.push({ id, method: 'PUT', path: '/api/admin/features/plannerReview', role: 'admin', raw: '{"enabled":true}', ...value });
for (const name of names) {
  for (const enabled of [true, false]) add(`${name}:${enabled}`, { path: `/api/admin/features/${name}`, raw: JSON.stringify({ enabled }) });
  add(`${name}:env-lock`, { path: `/api/admin/features/${name}`, env: { [original.REGISTRY[name].env]: 'true' }, raw: '{"enabled":false}' });
  if (original.REGISTRY[name].unavailable) {
    add(`${name}:unavailable-on`, { path: `/api/admin/features/${name}`, unavailable: name });
    add(`${name}:unavailable-off`, { path: `/api/admin/features/${name}`, unavailable: name, raw: '{"enabled":false}' });
  }
}
for (const method of ['GET', 'PUT', 'POST', 'HEAD', 'OPTIONS', 'DELETE']) {
  for (const [target, role] of [['/api/features', 'member'], ['/api/admin/features', 'admin'], ['/api/admin/features/plannerReview', 'member']]) {
    add(`${method}:${target}:${role}`, { method, path: target, role, bodyMode: 'unused-error' });
  }
}
for (const suffix of ['astraReview', '%70lannerReview', '', 'plannerReview/', 'plannerReview/x', '%2F', '%', '%C0%AF', 'UNKNOWN']) {
  for (const method of ['PUT', 'GET']) add(`suffix:${suffix}:${method}`, { path: `/api/admin/features/${suffix}`, method });
}
for (const raw of ['', '{', 'null', 'true', '17', '[]', '"text"', '{}', '{"enabled":null}', '{"enabled":0}', '{"enabled":"true"}', '{"enabled":true,"enabled":false}', '{"enabled":false,"enabled":true}', '{"enabled":true,"ignored":"\\ud800"}', '{"enabled":true,"ignored":1e400}', '{"enabled":1e400}', '{"en\\u0061bled":true}']) add(`json:${raw}`, { raw });
for (const bodyMode of ['stream-error', 'overflow', 'invalid-utf8']) add(`body:${bodyMode}`, { bodyMode });
add('unknown-before-type', { path: '/api/admin/features/unknown', raw: '{"enabled":"true"}' });
add('no-store', { noStore: true });
for (const fail of ['set', 'audit', 'onChange']) add(`failure:${fail}`, { fail });
for (const seed of [{ 'feature:astraReview': 'true' }, { 'feature:plannerReview': 'false', 'feature:astraReview': 'true' }, { 'feature:plannerReview': 'invalid', 'feature:astraReview': 'true' }]) {
  for (const fail of [null, 'migration']) add(`migration:${JSON.stringify(seed)}:${fail}`, { seed, fail, method: 'GET', path: '/api/admin/features' });
}
add('env-suppresses-migration', { seed: { 'feature:astraReview': 'true' }, env: { [original.REGISTRY.plannerReview.env]: 'false' }, method: 'GET', path: '/api/admin/features' });
add('invalid-startup-env', { env: { [original.REGISTRY.plannerReview.env]: 'invalid' } });
for (const name of ['diaryMcpWrite', 'kiwix']) {
  add(`${name}:selected-off-boot-on`, { path: `/api/admin/features/${name}`, seed: { [`feature:${name}`]: 'true' }, raw: '{"enabled":false}' });
  add(`${name}:new-boot`, { method: 'GET', path: '/api/features', seed: { [`feature:${name}`]: 'true' } });
}

async function run(spec) {
  const rows = new Map(Object.entries(spec.seed || {}));
  const writes = [], audits = [], changes = [], diagnostics = [];
  const store = { get: key => rows.get(key), set(key, value) {
    if (spec.fail === 'set' || (spec.fail === 'migration' && key === 'feature:plannerReview')) throw Error('synthetic write failure');
    rows.set(key, value); writes.push({ key, value });
  } };
  // Inject only runtime availability facts through the original factory's supported inputs.
  // This corpus does NOT claim to test Playwright, decision providers or producer context.
  const registry = Object.fromEntries(Object.entries(original.REGISTRY).map(([name, row]) => [name, { ...row, ...(row.unavailable ? { unavailable: () => spec.unavailable === name ? unavailableReasons[name] : null } : {}) }]));
  let features;
  try {
    features = original.createFeatures({ env: spec.env || {}, store: spec.noStore ? null : store, registry,
      audit: (...args) => { if (spec.fail === 'audit') throw Error('synthetic audit failure'); audits.push(args); },
      onChange: (...args) => { if (spec.fail === 'onChange') throw Error('synthetic onChange failure'); changes.push(args); },
      log: value => diagnostics.push(value),
    });
  } catch (error) {
    return { id: spec.id, input: spec, initializationError: { name: error.name, message: error.message }, writes, audits, changes, rows: [...rows].sort() };
  }
  let polls = 0, status = null, body = null, handled;
  const req = { method: spec.method, async *[Symbol.asyncIterator]() {
    polls++;
    if (spec.bodyMode === 'stream-error' || spec.bodyMode === 'unused-error') throw Error('synthetic body failure');
    if (spec.bodyMode === 'overflow') { yield Buffer.alloc(1024 * 1024 + 1, 0x20); return; }
    if (spec.bodyMode === 'invalid-utf8') { yield Buffer.from([0xff]); return; }
    yield Buffer.from(spec.raw);
  } };
  const routes = createFeatureRoutes({ features, readJson: http.readJson, json: (_res, code, value) => { status = code; body = value; } });
  try { handled = await routes(req, {}, { path: spec.path, authn: { user: { id: 'synthetic-admin', role: spec.role } } }); }
  catch (error) { const failure = http.errorResponse(error); status = failure.status; body = failure.body; handled = true; }
  return { id: spec.id, input: spec, handled, status, body, polls, writes, audits, changes, rows: [...rows].sort(), flags: features.flags(), diagnostics };
}
(async () => {
  const cases = [];
  for (const spec of specs) cases.push(await run(spec));
  const corpus = { schema: 1, coreRef: CORE_REF, source, scope: 'Original route module plus original errorResponse/readJson and injected synthetic availability; outer auth/browser/device gates and runtime producer facts need actual integration tests. Original partial audit/onChange failure traces are evidence; planned native atomic rollback is an explicit strengthening.', names, cases };
  const bytes = JSON.stringify(corpus, null, 2) + '\n';
  if (process.argv.includes('--check')) {
    if (!fs.existsSync(output) || fs.readFileSync(output, 'utf8') !== bytes) throw Error('Original feature HTTP corpus differs');
  } else fs.writeFileSync(output, bytes);
  console.log(`${cases.length} original Node feature HTTP/write cases ${process.argv.includes('--check') ? 'verified' : 'generated'}; no Rust import or runtime qualification`);
})().catch(error => { console.error(error); process.exitCode = 1; });
