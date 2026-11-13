'use strict';
// Records Core's own answers for POST /api/models/delete (server/routes/models.cjs, Core 76cc868) with scripted
// fakes, as the Rust test replays them:
//   CORE=<noevia-core checkout> node record-delete.cjs > delete-model-node.json
// Synthetic QA-named models only; nothing is contacted. `file` is whether the folder scan lists a file, `hasFile`
// what the model manager's sections listing says. `unload` and `delete` script the engine's answers in order
// (the last one repeats); removeModel is Core's llamacpp-manager.cjs algorithm over them.
const { Readable } = require('node:stream');
const { createModelRoutes } = require(`${process.env.CORE}/server/routes/models.cjs`);

const M = (name, extra = {}) => ({ name, file: true, hasFile: true, ...extra });
const OK = { status: 200, body: {} };
const GATE = (code, message, retryable = false) => ({ status: 409, body: { error: { type: 'gate_error', code, message, retryable } } });
const SCENARIOS = [
  { id: 'deletes-everything', name: 'qa-model-a', models: [M('qa-model-a'), M('qa-other')], unload: [OK], delete: [OK] },
  { id: 'not-loaded-unload-refused', name: 'qa-model-a', models: [M('qa-model-a'), M('qa-other')], unload: [{ status: 404, body: {} }], delete: [OK] },
  { id: 'delete-retried-after-second-unload', name: 'qa-model-a', models: [M('qa-model-a'), M('qa-other')], unload: [{ status: 404, body: {} }, OK], delete: [{ status: 500, body: {} }, OK] },
  { id: 'file-missing-section-only', name: 'qa-gone', models: [M('qa-gone', { file: false, hasFile: false }), M('qa-other')], unload: [{ status: 404, body: {} }], delete: [{ status: 404, body: {} }] },
  { id: 'file-missing-hold-refused', name: 'qa-gone', models: [M('qa-gone', { file: false, hasFile: false }), M('qa-other')], unload: [{ status: 404, body: {} }], delete: [{ status: 404, body: {} }], holdRefused: true },
  { id: 'deleted-hold-refused', name: 'qa-model-a', models: [M('qa-model-a'), M('qa-other')], unload: [OK], delete: [OK], holdRefused: true },
  { id: 'reload-fails', name: 'qa-model-a', models: [M('qa-model-a'), M('qa-other')], unload: [OK], delete: [OK], reloadOk: false },
  { id: 'section-delete-refused', name: 'qa-model-a', models: [M('qa-model-a'), M('qa-other')], unload: [OK], delete: [OK], refuseSection: true },
  { id: 'engine-delete-fails-502', name: 'qa-model-a', models: [M('qa-model-a'), M('qa-other')], unload: [OK], delete: [{ status: 500, body: { error: 'boom' } }] },
  { id: 'engine-refuses-model-in-use', name: 'qa-model-a', models: [M('qa-model-a'), M('qa-other')], unload: [GATE('model_in_use', 'qa-model-a is answering a request', true)], delete: [GATE('model_in_use', 'qa-model-a is answering a request', true)] },
  { id: 'engine-refuses-plain-shape', name: 'qa-model-a', models: [M('qa-model-a'), M('qa-other')], unload: [OK], delete: [{ status: 409, body: { error: 'The engine is busy.', code: 'engine_busy', retryable: true } }] },
  { id: 'engine-refuses-maintenance-held', name: 'qa-model-a', models: [M('qa-model-a'), M('qa-other')], unload: [OK], delete: [GATE('maintenance_held', 'A tune holds the engine.', true)] },
  { id: 'invalid-json', raw: '{not json', models: [M('qa-model-a')] },
  { id: 'name-required-empty-object', raw: '{}', models: [M('qa-model-a')] },
  { id: 'name-required-empty-name', raw: '{"name":""}', models: [M('qa-model-a')] },
  { id: 'name-required-array', raw: '[]', models: [M('qa-model-a')] },
  { id: 'null-body', raw: 'null', models: [M('qa-model-a')] },
  { id: 'system-model-refused', name: 'laya_multilingual_f16', models: [M('laya_multilingual_f16'), M('qa-other')] },
  { id: 'system-model-by-router-path', name: 'qa-neutral-id', models: [M('qa-neutral-id'), M('qa-other')], routerPath: '/models/laya_multilingual_f16.gguf' },
  { id: 'system-model-confirmed', name: 'laya_multilingual_f16', confirmSystem: true, models: [M('laya_multilingual_f16'), M('qa-other')], unload: [OK], delete: [OK] },
  { id: 'system-model-confirm-not-true', name: 'laya_multilingual_f16', body: { name: 'laya_multilingual_f16', confirmSystem: 'yes' }, models: [M('laya_multilingual_f16')] },
  { id: 'sidecar-embedding', name: 'qa-embed', models: [M('qa-embed'), M('qa-other')], env: { EMBEDDING_MODEL: 'qa-embed' }, sidecars: ['qa-embed'] },
  { id: 'sidecar-reranker', name: 'qa-rerank', models: [M('qa-rerank'), M('qa-other')], env: { NOEVIA_FEATURE_RAG_RERANK: 'on', RERANK_MODEL: 'qa-rerank' }, sidecars: ['qa-rerank'] },
  { id: 'disabled', name: 'qa-model-a', models: [], disabled: true },
  { id: 'listing-unreadable', name: 'qa-model-a', models: [M('qa-model-a')], listing: 'fail' },
  { id: 'not-found', name: 'qa-nope', models: [M('qa-model-a'), M('qa-other')] },
  { id: 'installed-list-unreadable-still-deletes', name: 'qa-model-a', models: [M('qa-model-a'), M('qa-other')], installedFails: true, unload: [OK], delete: [OK] },
  { id: 'role-cleanup-throws', name: 'qa-model-a', models: [M('qa-model-a'), M('qa-other')], unload: [OK], delete: [OK], rolesThrow: true },
];

function pick(list, n) { return list[Math.min(n, list.length - 1)]; }

async function run(sc) {
  const fetched = [], engine = [], reloads = [], roles = [], forgot = [];
  let sections = sc.models.map((m) => ({ name: m.name, hasFile: m.hasFile }));
  const out = [];
  const installed = sc.models.map((m) => ({ name: m.name, labels: [], loaded: false, missingFile: !m.file, source: 'preset' }));
  const scan = sc.models.filter((m) => m.file).map((m) => ({ modelId: m.name, sections: [m.name] }));
  let unloads = 0, deletes = 0;
  const fake = async (reply) => ({ ok: reply.status < 400, status: reply.status, body: reply.body });
  const modelManager = {
    enabled: !sc.disabled, kind: 'llamacpp', capabilities: {},
    // The listing the second system/sidecar check reads (the router's own `/models`).
    request: async (path) => {
      engine.push(`GET ${path}`);
      if (sc.listing === 'fail') return { ok: false, status: 500, body: {} };
      return { ok: true, status: 200, body: { data: sc.models.map((m) => ({ id: m.name, status: { value: 'unloaded', args: ['--model', m.name === sc.name && sc.routerPath ? sc.routerPath : `/models/${m.name}.gguf`] } })) } };
    },
    // llamacpp-manager.cjs removeModel over the scripted engine answers.
    removeModel: async (model) => {
      const unload = async () => { engine.push('POST /models/unload'); return fake(pick(sc.unload, unloads++)); };
      const del = async () => { engine.push(`DELETE /models?model=${encodeURIComponent(model)}`); return fake(pick(sc.delete, deletes++)); };
      let unloaded = !!(await unload()).ok;
      let r = await del();
      if (!r.ok) { if ((await unload()).ok) unloaded = true; r = await del(); }
      return { ...r, unloaded };
    },
    forgetIdentity: (n) => forgot.push(n),
    writePresets: async (write, reloadIf) => {
      if (sc.holdRefused) throw Object.assign(Error('busy'), { status: 409, holdRefused: true });
      const written = await write();
      if (!reloadIf(written)) return { written, reload: null };
      reloads.push('reload');
      return { written, reload: { ok: sc.reloadOk !== false } };
    },
  };
  const body = sc.body || (sc.raw !== undefined ? null : { name: sc.name, ...(sc.confirmSystem ? { confirmSystem: true } : {}) });
  const handle = createModelRoutes({
    servedEmbeddingModels: async () => null,
    json: (res, status, b) => { out.push({ status, body: b }); },
    readBody: async () => (sc.raw !== undefined ? sc.raw : JSON.stringify(body)), readJson: async () => ({}),
    fetchJson: async (url, init) => {
      fetched.push(`${init.method} ${url.replace('http://loader.invalid/api/v1/', '')}`);
      if (url.endsWith('/sections') && init.method === 'GET') return { ok: true, status: 200, body: { revision: `r${sections.length}`, sections } };
      const m = /sections\/([^?]+)\?baseRevision=r(\d+)$/.exec(url);
      if (m && init.method === 'DELETE' && !sc.refuseSection && Number(m[2]) === sections.length) { sections = sections.filter((x) => x.name !== decodeURIComponent(m[1])); return { ok: true, status: 200, body: {} }; }
      return { ok: false, status: 409, body: {} };
    },
    env: { MODEL_LOADER_URL: 'http://loader.invalid', ...(sc.env || {}) }, modelManager,
    getProvider: () => ({}), providerHeaders: () => ({}), DEFAULT_PROVIDER_ID: 'default',
    createVisionProbe: () => async () => ({ supported: true }),
    missingRoles: () => null, currentWorkspace: () => ({ userId: 'u1' }),
    service: { modelScanCache: new Map(), refreshModelScan: () => {}, autoRoles: () => null, setAutoRoles: () => {}, ensureRolesLoaded: () => {},
      servedCatalogue: async () => [], modelsInstalled: async () => installed,
      modelsInstalledFresh: async () => { if (sc.installedFails) throw new Error('engine down'); return installed; },
      deriveUserModelName: (c) => c, lastLoadedModel: () => null, clearLastLoadedModel: () => {},
      clearRoleReferences: async (n) => { roles.push(n); if (sc.rolesThrow) throw new Error('disk'); return []; },
      invalidateModelReads: () => {} },
  });
  const req = Readable.from([]);
  req.method = 'POST';
  let threw = null;
  try {
    await handle(req, { setHeader() {} }, { path: '/api/models/delete', authn: { user: { id: 'u1', role: 'admin' } }, url: new URL('http://localhost/api/models/delete') });
  } catch (e) { threw = String(e.message || e); }
  const answer = out.pop() || null;
  return { id: sc.id, answer, threw, loader: fetched, engine, reloads: reloads.length, rolesCleared: roles, sectionsAfter: sections.map((s) => s.name), scenario: sc };
}

(async () => {
  const cases = [];
  for (const sc of SCENARIOS) cases.push(await run(sc));
  process.stdout.write(`${JSON.stringify({ coreRef: '76cc868', cases }, null, 2)}\n`);
})();
