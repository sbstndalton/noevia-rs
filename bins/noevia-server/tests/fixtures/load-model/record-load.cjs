'use strict';
// Records Core's own answers for POST /api/models/load and /api/models/unload (server/routes/models.cjs, Core 76cc868)
// with Core's real llamacpp manager and inference-gate client over scripted transports, as the Rust test replays them:
//   CORE=<noevia-core checkout> node record-load.cjs > load-model-node.json
// Synthetic QA-named models only; nothing is contacted. The router and the gate are scripted fetches; the budget
// fence is a temp directory holding the watchdog's own fence file.
const { Readable } = require('node:stream');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { createHash } = require('node:crypto');
const CORE = process.env.CORE;
const { createModelRoutes } = require(`${CORE}/server/routes/models.cjs`);
const { createLlamaCppManager } = require(`${CORE}/server/llamacpp-manager.cjs`);
const { createGateClient } = require(`${CORE}/server/inference-gate-client.cjs`);

const L = (name, status = 'unloaded', extra = {}) => ({ name, status, file: true, ...extra });
const GATE = (code, message, retryable = false) => ({ error: { type: 'gate_error', code, message, retryable } });
const OK = { status: 200, body: {} };
const SSE = (...events) => ({ sse: events });
const GRANTED = SSE(['queued', { position: 2, waited_ms: 10 }], ['granted', { lease: 'a'.repeat(32) }]);
const SCENARIOS = [
  // ── load ──
  { id: 'load-success', verb: 'load', name: 'qa-model-a', models: [L('qa-model-a')], lease: GRANTED, load: [OK], after: ['loading', 'loaded'] },
  { id: 'load-already-loaded', verb: 'load', name: 'qa-model-a', models: [L('qa-model-a', 'loaded')], lease: GRANTED, load: [{ status: 400, body: { error: 'model is already running' } }], after: ['loaded'] },
  { id: 'load-sleeping-model', verb: 'load', name: 'qa-model-a', models: [L('qa-model-a', 'sleeping')], lease: GRANTED, load: [OK], after: ['loaded'] },
  { id: 'load-no-gate-proceeds', verb: 'load', name: 'qa-model-a', models: [L('qa-model-a')], lease: { status: 404, body: {} }, load: [OK], after: ['loaded'] },
  { id: 'gate-queue-full', verb: 'load', name: 'qa-model-a', models: [L('qa-model-a')], lease: { status: 429, body: GATE('inference_queue_full', 'The queue is full.', true) } },
  { id: 'gate-unknown-model', verb: 'load', name: 'qa-nope', models: [L('qa-model-a')], lease: { status: 404, body: GATE('model_not_found', 'No such model.') } },
  { id: 'gate-maintenance-held', verb: 'load', name: 'qa-model-a', models: [L('qa-model-a')], lease: SSE(['queued', { position: 1, waited_ms: 5 }], ['error', { code: 'maintenance_held', retryable: true, message: 'A tune holds the engine.' }]) },
  { id: 'gate-unreachable', verb: 'load', name: 'qa-model-a', models: [L('qa-model-a')], lease: { throws: true } },
  { id: 'gate-lease-garbled', verb: 'load', name: 'qa-model-a', models: [L('qa-model-a')], lease: SSE(['granted', { lease: 'nope' }]) },
  { id: 'budget-fence-refused', verb: 'load', name: 'qa-model-a', models: [L('qa-model-a')], lease: GRANTED, fence: true, load: [OK], after: ['loaded'] },
  { id: 'engine-load-refused-500', verb: 'load', name: 'qa-model-a', models: [L('qa-model-a')], lease: GRANTED, load: [{ status: 500, body: { error: 'boom' } }] },
  { id: 'engine-load-gate-refusal', verb: 'load', name: 'qa-model-a', models: [L('qa-model-a')], lease: GRANTED, load: [{ status: 409, body: GATE('engine_busy', 'The engine is busy.', true) }] },
  { id: 'never-becomes-ready', verb: 'load', name: 'qa-model-a', models: [L('qa-model-a')], lease: GRANTED, load: [OK], after: ['unloaded'] },
  { id: 'router-listing-unreadable', verb: 'load', name: 'qa-model-a', models: [L('qa-model-a')], lease: GRANTED, listing: 'fail', load: [OK] },
  { id: 'load-installed-list-unreadable-still-loads', verb: 'load', name: 'qa-model-a', models: [L('qa-model-a')], lease: GRANTED, load: [OK], after: ['loaded'], installedFails: true },
  { id: 'load-missing-file', verb: 'load', name: 'qa-gone', models: [L('qa-gone', 'unloaded', { file: false }), L('qa-other')], lease: GRANTED },
  { id: 'load-system-model', verb: 'load', name: 'laya_multilingual_f16', models: [L('laya_multilingual_f16')], lease: GRANTED },
  { id: 'load-sidecar-model', verb: 'load', name: 'qa-embed', models: [L('qa-embed')], env: { EMBEDDING_MODEL: 'qa-embed' }, lease: GRANTED },
  { id: 'load-mtp-on-llamacpp', verb: 'load', name: 'qa-model-a', body: { name: 'qa-model-a', mtp: true }, models: [L('qa-model-a')], lease: GRANTED },
  { id: 'load-disabled', verb: 'load', name: 'qa-model-a', models: [], disabled: true },
  { id: 'load-invalid-json', verb: 'load', raw: '{not json', models: [] },
  { id: 'load-name-required-empty-object', verb: 'load', raw: '{}', models: [] },
  { id: 'load-name-required-empty-name', verb: 'load', raw: '{"name":""}', models: [] },
  { id: 'load-null-body', verb: 'load', raw: 'null', models: [] },
  // ── unload ──
  { id: 'unload-success', verb: 'unload', name: 'qa-model-a', models: [L('qa-model-a', 'loaded')], unload: [OK] },
  { id: 'unload-not-loaded', verb: 'unload', name: 'qa-model-a', models: [L('qa-model-a')], unload: [{ status: 404, body: {} }] },
  { id: 'unload-in-use-can-force', verb: 'unload', name: 'qa-model-a', models: [L('qa-model-a', 'loaded')], admin: true, unload: [{ status: 409, body: GATE('model_in_use', 'qa-model-a is answering a request', true) }] },
  { id: 'unload-in-use-cannot-force', verb: 'unload', name: 'qa-model-a', models: [L('qa-model-a', 'loaded')], unload: [{ status: 409, body: GATE('model_in_use', 'qa-model-a is answering a request', true) }] },
  { id: 'unload-force-success', verb: 'unload', name: 'qa-model-a', body: { name: 'qa-model-a', force: true }, models: [L('qa-model-a', 'loaded')], admin: true, unload: [OK] },
  { id: 'unload-force-without-admin', verb: 'unload', name: 'qa-model-a', body: { name: 'qa-model-a', force: true }, models: [L('qa-model-a', 'loaded')], unload: [OK] },
  { id: 'unload-force-refused', verb: 'unload', name: 'qa-model-a', body: { name: 'qa-model-a', force: true }, models: [L('qa-model-a', 'loaded')], admin: true, unload: [{ status: 409, body: GATE('engine_busy', 'The engine is busy.', true) }] },
  { id: 'unload-force-engine-fails', verb: 'unload', name: 'qa-model-a', body: { name: 'qa-model-a', force: true }, models: [L('qa-model-a', 'loaded')], admin: true, unload: [{ status: 500, body: {} }] },
  { id: 'unload-force-not-true', verb: 'unload', name: 'qa-model-a', body: { name: 'qa-model-a', force: 'yes' }, models: [L('qa-model-a', 'loaded')], unload: [OK] },
  { id: 'unload-engine-fails-502', verb: 'unload', name: 'qa-model-a', models: [L('qa-model-a', 'loaded')], unload: [{ status: 500, body: {} }] },
  { id: 'unload-unknown-model', verb: 'unload', name: 'qa-nope', models: [L('qa-model-a')], unload: [{ status: 404, body: GATE('model_not_found', 'No such model.') }] },
  { id: 'unload-system-model', verb: 'unload', name: 'laya_multilingual_f16', models: [L('laya_multilingual_f16', 'loaded')] },
  { id: 'unload-sidecar-model', verb: 'unload', name: 'qa-embed', models: [L('qa-embed', 'loaded')], env: { EMBEDDING_MODEL: 'qa-embed' } },
  { id: 'unload-missing-file-allowed', verb: 'unload', name: 'qa-gone', models: [L('qa-gone', 'loaded', { file: false }), L('qa-other')], unload: [OK] },
  { id: 'unload-disabled', verb: 'unload', name: 'qa-model-a', models: [], disabled: true },
  { id: 'unload-null-body', verb: 'unload', raw: 'null', models: [] },
  { id: 'unload-invalid-json', verb: 'unload', raw: '[', models: [] },
];

// Core logs its own events with console.log; stdout is the recording.
console.log = (...a) => console.error(...a);
function pick(list, n) { return list[Math.min(n, list.length - 1)]; }
const sseResponse = (events) => new Response(events.map(([e, d]) => `event: ${e}\ndata: ${JSON.stringify(d)}\n\n`).join(''), { status: 200, headers: { 'content-type': 'text/event-stream' } });

async function run(sc) {
  const engine = [], gateLog = [], out = [];
  let loads = 0, unloads = 0, polls = 0, released = 0;
  const fence = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-fence-'));
  if (sc.fence) {
    const hash = createHash('sha256').update(sc.name).digest('hex');
    fs.writeFileSync(path.join(fence, `${hash}.json`), JSON.stringify({ model: sc.name, revision: null, budgetGib: 8, at: 1700000000000 }));
  }
  const rows = () => sc.models.map((m) => {
    const after = sc.after ? pick(sc.after, polls) : null;
    const value = loads > 0 && after ? after : m.status;
    return { id: m.name, status: { value, args: ['--model', `/models/${m.name}.gguf`] } };
  });
  const fakeEngine = async (url, init = {}) => {
    const p = url.replace('http://router.invalid', '');
    const method = (init.method || 'GET').toUpperCase();
    if (method === 'GET' && p.startsWith('/models')) {
      engine.push(`GET ${p}`);
      if (sc.listing === 'fail') return { ok: false, status: 500, body: {} };
      if (loads > 0) polls++;
      return { ok: true, status: 200, body: { data: rows() } };
    }
    if (method === 'POST' && p === '/models/load') {
      engine.push(`POST ${p} ${init.body} lease=${init.headers?.['X-Noevia-Lease'] ? 'yes' : 'no'}`);
      const r = pick(sc.load || [OK], loads++);
      return { ok: r.status < 400, status: r.status, body: r.body };
    }
    if (method === 'POST' && p === '/models/unload') {
      engine.push(`POST ${p} ${init.body} admin=${init.headers?.['X-Noevia-Gate-Admin'] ? 'yes' : 'no'}`);
      const r = pick(sc.unload || [OK], unloads++);
      return { ok: r.status < 400, status: r.status, body: r.body };
    }
    return { ok: false, status: 404, body: {} };
  };
  const gate = createGateClient({
    baseUrl: 'http://router.invalid', adminToken: sc.admin ? 'synthetic-admin-token' : null, log: () => {},
    fetch: async (url, init = {}) => {
      const p = url.replace('http://router.invalid', '');
      if (init.method === 'DELETE') { released++; gateLog.push(`DELETE ${p}`); return new Response(null, { status: 204 }); }
      gateLog.push(`POST ${p} ${init.body}`);
      const l = sc.lease;
      if (l.throws) throw new Error('gate down');
      if (l.sse) return sseResponse(l.sse);
      return new Response(JSON.stringify(l.body), { status: l.status, headers: { 'content-type': 'application/json' } });
    },
  });
  const manager = createLlamaCppManager({
    baseUrl: 'http://router.invalid', fetchJson: fakeEngine, gate, gateAdminToken: sc.admin ? 'synthetic-admin-token' : null,
    budgetWatchDir: fence, inferenceBudget: { budgetGib: () => 8 },
  });
  const installed = sc.models.map((m) => ({ name: m.name, labels: [], loaded: false, missingFile: !m.file, source: 'preset' }));
  const body = sc.body || (sc.raw !== undefined ? null : { name: sc.name });
  const handle = createModelRoutes({
    servedEmbeddingModels: async () => null,
    json: (res, status, b) => { out.push({ status, body: b }); },
    readBody: async () => (sc.raw !== undefined ? sc.raw : JSON.stringify(body)), readJson: async () => ({}),
    // The sweep guard's boot check asks the model manager for a running sweep: none.
    fetchJson: async () => ({ ok: true, status: 200, body: { job: null } }),
    env: { MODEL_LOADER_URL: 'http://loader.invalid', ...(sc.env || {}) },
    modelManager: sc.disabled ? { ...manager, enabled: false } : manager,
    getProvider: () => ({}), providerHeaders: () => ({}), DEFAULT_PROVIDER_ID: 'default',
    createVisionProbe: () => async () => ({ supported: true }),
    missingRoles: () => null, currentWorkspace: () => ({ userId: 'u1' }),
    service: { modelScanCache: new Map(), refreshModelScan: () => {}, autoRoles: () => null, setAutoRoles: () => {}, ensureRolesLoaded: () => {},
      servedCatalogue: async () => [], modelsInstalled: async () => installed,
      modelsInstalledFresh: async () => { if (sc.installedFails) throw new Error('engine down'); return installed; },
      deriveUserModelName: (c) => c, lastLoadedModel: () => null, clearLastLoadedModel: () => {}, clearRoleReferences: async () => [], invalidateModelReads: () => {} },
  });
  await handle.sweepAdopted;
  const req = Readable.from([]);
  req.method = 'POST';
  const res = new (require('node:events'))();
  res.setHeader = () => {}; res.writableEnded = false; res.off = res.removeListener;
  let threw = null;
  const p = `/api/models/${sc.verb}`;
  try { await handle(req, res, { path: p, authn: { user: { id: 'u1', role: 'admin' } }, url: new URL(`http://localhost${p}`) }); }
  catch (e) { threw = String(e.message || e); }
  fs.rmSync(fence, { recursive: true, force: true });
  return { id: sc.id, answer: out.pop() || null, threw, engine, gate: gateLog, leasesReleased: released, scenario: sc };
}

(async () => {
  const cases = [];
  for (const sc of SCENARIOS) cases.push(await run(sc));
  process.stdout.write(`${JSON.stringify({ coreRef: '76cc868', cases }, null, 2)}\n`);
})();
