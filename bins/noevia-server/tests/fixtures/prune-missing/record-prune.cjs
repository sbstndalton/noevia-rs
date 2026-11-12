'use strict';
// Records Core's own answers for POST /api/models/prune-missing (server/routes/models.cjs
// pruneMissingPresets, Core 6c23568d) with scripted fakes, as the Rust test replays them:
//   CORE=<noevia-core checkout> node record-prune.cjs > prune-missing-node.json
// Each scenario names synthetic models (QA names); `file` is whether the folder scan lists a file, `hasFile`
// what the model manager's sections listing says. Synthetic data only; nothing is contacted.
const { Readable } = require('node:stream');
const { createModelRoutes } = require(`${process.env.CORE}/server/routes/models.cjs`);

const SCENARIOS = [
  { id: 'removes-missing-and-reloads', models: [
    { name: 'qa-gone-a', file: false, hasFile: false }, { name: 'qa-gone-b', file: false, hasFile: false },
    { name: 'qa-scan-only', file: false, hasFile: true }, { name: 'qa-kept-1', file: true, hasFile: true },
    { name: 'qa-kept-2', file: true, hasFile: true }, { name: 'qa-kept-3', file: true, hasFile: true }] },
  { id: 'nothing-missing', models: [{ name: 'qa-kept-1', file: true, hasFile: true }, { name: 'qa-kept-2', file: true, hasFile: true }] },
  { id: 'empty-scan', models: [{ name: 'qa-a', file: false, hasFile: false }, { name: 'qa-b', file: false, hasFile: false }], scan: [] },
  { id: 'more-than-half', models: [{ name: 'qa-a', file: false, hasFile: false }, { name: 'qa-b', file: false, hasFile: false }, { name: 'qa-c', file: false, hasFile: false },
    { name: 'qa-d', file: true, hasFile: true }, { name: 'qa-e', file: true, hasFile: true }] },
  { id: 'exactly-half', models: [{ name: 'qa-a', file: false, hasFile: false }, { name: 'qa-b', file: true, hasFile: true }] },
  { id: 'system-and-served-elsewhere-skipped', models: [
    { name: 'laya_multilingual_f16', file: false, hasFile: false, system: true }, { name: 'qa-away', file: false, hasFile: false, servedElsewhere: true },
    { name: 'qa-cached', file: false, hasFile: false, source: 'cache' }, { name: 'qa-gone', file: false, hasFile: false },
    { name: 'qa-k1', file: true, hasFile: true }, { name: 'qa-k2', file: true, hasFile: true }, { name: 'qa-k3', file: true, hasFile: true },
    { name: 'qa-k4', file: true, hasFile: true }, { name: 'qa-k5', file: true, hasFile: true }, { name: 'qa-k6', file: true, hasFile: true }] },
  { id: 'delete-refused-is-kept', models: [{ name: 'qa-gone', file: false, hasFile: false }, { name: 'qa-k1', file: true, hasFile: true }, { name: 'qa-k2', file: true, hasFile: true }], refuseDelete: true },
  { id: 'hold-refused', models: [{ name: 'qa-gone', file: false, hasFile: false }, { name: 'qa-k1', file: true, hasFile: true }, { name: 'qa-k2', file: true, hasFile: true }], holdRefused: true },
  { id: 'reload-fails', models: [{ name: 'qa-gone', file: false, hasFile: false }, { name: 'qa-k1', file: true, hasFile: true }, { name: 'qa-k2', file: true, hasFile: true }], reloadOk: false },
  { id: 'disabled', models: [], disabled: true },
];

async function run(sc) {
  const fetched = [], reloads = [], roles = [], forgot = [];
  let sections = sc.models.map((m) => ({ name: m.name, hasFile: m.hasFile }));
  const out = [];
  const installed = sc.models.map((m) => ({ name: m.name, labels: [], missingFile: !m.file, ...(m.system ? { system: true } : {}),
    ...(m.servedElsewhere ? { servedElsewhere: true } : {}), ...(m.source ? { source: m.source } : { source: 'preset' }) }));
  const scan = sc.scan || sc.models.filter((m) => m.file).map((m) => ({ modelId: m.name, sections: [m.name] }));
  const modelManager = { enabled: !sc.disabled, kind: 'llamacpp', capabilities: {},
    writePresets: async (write, reloadIf) => {
      if (sc.holdRefused) throw Object.assign(Error('busy'), { status: 409, holdRefused: true });
      const written = await write();
      if (!reloadIf(written)) return { written, reload: null };
      reloads.push('reload');
      return { written, reload: { ok: sc.reloadOk !== false } };
    } };
  const handle = createModelRoutes({
    servedEmbeddingModels: async () => null,
    json: (res, status, body) => { out.push({ status, body }); },
    readBody: async () => '', readJson: async () => ({}),
    fetchJson: async (url, init) => {
      fetched.push(`${init.method} ${url.replace('http://loader.invalid/api/v1/', '')}`);
      if (url.endsWith('/api/v1/models') && init.method === 'GET') return { ok: true, status: 200, body: { models: scan } };
      if (url.endsWith('/sections') && init.method === 'GET') return { ok: true, status: 200, body: { revision: `r${sections.length}`, sections } };
      const m = /sections\/([^?]+)\?baseRevision=r(\d+)$/.exec(url);
      if (m && init.method === 'DELETE' && !sc.refuseDelete && Number(m[2]) === sections.length) { sections = sections.filter((x) => x.name !== decodeURIComponent(m[1])); return { ok: true, status: 200, body: {} }; }
      return { ok: false, status: 409, body: {} };
    },
    env: { MODEL_LOADER_URL: 'http://loader.invalid', RERANK_MODEL: 'qa-reranker' }, modelManager,
    getProvider: () => ({}), providerHeaders: () => ({}), DEFAULT_PROVIDER_ID: 'default',
    createVisionProbe: () => async () => ({ supported: true }),
    missingRoles: () => null, currentWorkspace: () => ({ userId: 'u1' }),
    service: { modelScanCache: new Map(), refreshModelScan: () => {}, autoRoles: () => null, setAutoRoles: () => {}, ensureRolesLoaded: () => {},
      servedCatalogue: async () => [], modelsInstalled: async () => installed, modelsInstalledFresh: async () => installed, deriveUserModelName: (c) => c,
      lastLoadedModel: () => null, clearLastLoadedModel: () => {}, clearRoleReferences: (n) => { roles.push(n); return []; },
      forgetModelState: async (n) => { forgot.push(n); } },
  });
  const req = Readable.from([]);
  req.method = 'POST';
  await handle(req, { setHeader() {} }, { path: '/api/models/prune-missing', authn: { user: { id: 'u1', role: 'admin' } }, url: new URL('http://localhost/api/models/prune-missing') });
  const answer = out.pop();
  return { id: sc.id, answer, loader: fetched, reloads: reloads.length, rolesCleared: roles, forgotten: forgot, sectionsAfter: sections.map((s) => s.name), scenario: sc };
}

(async () => {
  const cases = [];
  for (const sc of SCENARIOS) cases.push(await run(sc));
  process.stdout.write(`${JSON.stringify({ coreRef: '6c23568d', cases }, null, 2)}\n`);
})();
