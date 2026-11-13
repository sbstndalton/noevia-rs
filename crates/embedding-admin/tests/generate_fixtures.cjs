#!/usr/bin/env node
'use strict';
// Differential fixtures for crates/embedding-admin, recorded from noevia-core's own
// server/routes/embedding.cjs (createEmbeddingRoutes), wholly synthetic.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/embedding-admin/tests/generate_fixtures.cjs > crates/embedding-admin/tests/fixtures/node-embedding.v1.json
//
// Each case is a raw POST /api/embedding body (parsed by JSON.parse, as server/http.cjs readJson
// does) sent by a synthetic administrator through the real route. Its dependencies are stubs that
// record what the route asked of them: the address check (always passes), the served-model probe
// (always lists the model), the projects migration and the Diary relay (each answers as the case
// says). The Diary base is a .invalid name; nothing touches the network or a data directory.
// The source files' sha256 are checked, so a changed Node file needs a fresh recording.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const server = path.join(path.resolve(checkout), 'server');
const SOURCES = {
  'server/routes/embedding.cjs': 'ae34b6be30f2b8880a5c6c0c141ed9d1e709e87ef92202bb3cc0e79465db9854',
  'server/embedding-served.cjs': '9ab1c0d06c60d53beef408a887c5ca4f090451e91457ae50f89ff7c029054f3b',
};
for (const [rel, want] of Object.entries(SOURCES)) {
  const got = crypto.createHash('sha256').update(fs.readFileSync(path.join(checkout, rel))).digest('hex');
  if (got !== want) {
    process.stderr.write(`source hash mismatch for ${rel}: ${got}\n`);
    process.exit(1);
  }
}
const { createEmbeddingRoutes } = require(path.join(server, 'routes', 'embedding.cjs'));

// Request-reading cases: raw body text. The migration and the Diary both succeed.
const READS = [
  'null', '[]', '"switch"', '3', 'true', '{}',
  '{"action":""}', '{"action":false}', '{"action":0}', '{"action":7}', '{"action":"SWITCH"}', '{"action":" switch"}',
  '{"action":["switch"]}', '{"action":["switch","finish"]}', '{"action":{"a":1}}', '{"action":null}',
  '{"action":"switch"}', '{"action":"switch","force":true}', '{"action":"switch","force":"true"}', '{"action":"switch","force":1}',
  '{"action":"clear-next"}', '{"action":"rollback"}', '{"action":"finish"}',
  '{"action":"finish","scope":"projects"}', '{"action":"finish","scope":"diary"}', '{"action":"finish","scope":"all"}',
  '{"action":"finish","scope":""}', '{"action":"finish","scope":0}', '{"action":"finish","scope":false}', '{"action":"finish","scope":null}',
  '{"action":"finish","scope":"Projects"}', '{"action":"finish","scope":"none"}', '{"action":"finish","scope":["diary"]}',
  '{"action":"finish","scope":1}', '{"action":"finish","scope":{}}',
  '{"action":"service-off","scope":"nonsense"}', '{"action":"service-switch","scope":7,"model":"m"}',
  '{"action":"set-next"}', '{"action":"set-next","target":null}', '{"action":"set-next","target":"x"}',
  '{"action":"set-next","target":5}', '{"action":"set-next","target":true}', '{"action":"set-next","target":false}',
  '{"action":"set-next","target":{}}', '{"action":"set-next","target":[]}',
  '{"action":"set-next","target":{"model":"qa-embed"}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":""}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","target":{"model":" qa-embed ","baseUrl":"  http://10.0.0.5:8080  "}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":"http://10.0.0.5:8080","diaryBaseUrl":"http://10.0.0.6:8080"}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":"http://10.0.0.5:8080","diaryBaseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":"http://10.0.0.5:8080","diaryBaseUrl":" http://10.0.0.5:8080 "}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":"http://10.0.0.5:8080","diaryBaseUrl":""}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":"http://10.0.0.5:8080","diaryBaseUrl":"   "}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":"http://10.0.0.5:8080","diaryBaseUrl":null}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":"","diaryBaseUrl":"http://10.0.0.6:8080"}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":null,"diaryBaseUrl":"http://10.0.0.6:8080"}}',
  '{"action":"set-next","scope":"projects","target":{"model":"qa-embed"}}',
  '{"action":"set-next","scope":"projects","target":{"model":"qa-embed","baseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","scope":"diary","target":{"model":"qa-embed","baseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","scope":"diary","target":{"model":"qa-embed"}}',
  '{"action":"set-next","scope":"diary","target":{"model":"qa-embed","diaryBaseUrl":"http://10.0.0.6:8080"}}',
  '{"action":"set-next","scope":"projects","target":{"model":"qa-embed","diaryBaseUrl":""}}',
  '{"action":"set-next","target":{"model":12,"baseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","target":{"model":1.5,"baseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","target":{"model":1.0,"baseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","target":{"model":-0,"baseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","target":{"model":1e21,"baseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","target":{"model":12345678901234567890,"baseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","target":{"model":0.1,"baseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","target":{"model":false,"baseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","target":{"model":true,"baseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","target":{"model":["a","b"],"baseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","target":{"model":{"x":1},"baseUrl":"http://10.0.0.5:8080"}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":5}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":0}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":-0}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":false}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":true}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":{"a":1}}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":["http://10.0.0.5:8080"]}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":[]}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":"http://10.0.0.5:8080","diaryBaseUrl":0}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":"http://10.0.0.5:8080","diaryBaseUrl":false}}',
  '{"action":"set-next","target":["qa-embed"]}',
  '{"action":"set-next","target":{"model":"qa\\u00e9mbed","baseUrl":"\\u00a0http://10.0.0.5:8080\\u2003"}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":"\\ufeffhttp://10.0.0.5:8080"}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":"http://10.0.0.5:8080\\u0085"}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":"http://10.0.0.5:8080\\u180e"}}',
  '{"action":"set-next","target":{"model":"qa-embed","baseUrl":"\\u200bhttp://10.0.0.5:8080"}}',
  '{"action":"set-next","force":true,"scope":"projects","target":{"model":"qa-embed","baseUrl":"http://10.0.0.5:8080"}}',
];

// Aggregation cases: action clear-next at a scope, with each half answering as given.
// projects: 'ok' | {error, status?}; diary: 'ok' | {status, body} (an HTTP answer) | 'throw'.
const HALVES = [
  ['ok', 'ok'],
  [{ error: 'p', status: 409 }, 'ok'],
  ['ok', { status: 500, body: { error: 'd' } }],
  [{ error: 'p', status: 409 }, { status: 503, body: { error: 'd' } }],
  [{ error: 'p', status: 404 }, { status: 503, body: { detail: 'd' } }],
  [{ error: 'p', status: 302 }, { status: 503, body: {} }],
  [{ error: 'p', status: 600 }, 'throw'],
  [{ error: 'p', status: 399 }, 'ok'],
  [{ error: 'p' }, 'throw'],
  ['ok', 'throw'],
  [{ error: 'p', status: 418 }, { status: 400, body: { error: 'd', code: 'x' } }],
  [{ error: 'p', status: 599 }, { status: 404, body: { error: '' } }],
];

const synthetic = { user: { id: 'qa-admin-0001', role: 'admin' } };

async function run(raw, projectsHalf = 'ok', diaryHalf = 'ok') {
  const calls = { checkAddress: [], served: [], projects: [], diary: [] };
  let answer = null;
  const json = (_res, status, body) => { answer = { status, body }; };
  const failing = (name) => async (...args) => {
    calls.projects.push({ method: name, args });
    if (projectsHalf === 'ok') return { ok: true };
    const err = new Error(projectsHalf.error);
    if (projectsHalf.status !== undefined) err.status = projectsHalf.status;
    throw err;
  };
  const migration = {
    setNext: failing('setNext'), clearNext: failing('clearNext'), switchToNext: failing('switchToNext'),
    rollback: failing('rollback'), finish: failing('finish'),
    status: () => ({ active: null, next: null, previous: null }),
  };
  const fetchJson = async (url, init) => {
    calls.diary.push({ method: init.method, body: init.body === undefined ? null : JSON.parse(init.body) });
    if (diaryHalf === 'throw') throw new Error('synthetic Diary unreachable');
    if (diaryHalf === 'ok') return { ok: true, status: 200, body: { ok: true } };
    return { ok: false, status: diaryHalf.status, body: diaryHalf.body };
  };
  const route = createEmbeddingRoutes({
    json,
    readJson: async () => JSON.parse(raw),
    migration,
    authService: { audit() {} },
    fetchJson,
    DIARY_BASE: 'http://diary.synthetic.invalid',
    checkAddress: async (address) => { calls.checkAddress.push(address); return { ok: true }; },
    servedModels: async (target) => { calls.served.push(target.baseUrl); return [target.model]; },
  });
  const res = { setHeader() {} };
  await route({ method: 'POST' }, res, { path: '/api/embedding', authn: synthetic });
  return { status: answer.status, body: answer.body, calls };
}

(async () => {
  const reads = [];
  for (const raw of READS) {
    const r = await run(raw);
    reads.push({
      raw,
      status: r.status,
      error: typeof r.body?.error === 'string' ? r.body.error : null,
      code: r.body?.code ?? null,
      check_address: r.calls.checkAddress,
      served_probes: r.calls.served,
      served_model: null,
      projects: r.calls.projects.map((c) => c.method),
      switch_force: r.calls.projects.filter((c) => c.method === 'switchToNext').map((c) => c.args[0].force),
      diary: r.calls.diary.map((c) => c.body),
    });
  }
  // The model each probe asked for (the trimmed, coerced target model): the same for every probe.
  for (const [i, raw] of READS.entries()) {
    const models = [];
    const route = createEmbeddingRoutes({
      json() {}, readJson: async () => JSON.parse(raw),
      migration: { setNext: async () => ({}), clearNext: async () => ({}), switchToNext: async () => ({}), rollback: async () => ({}), finish: async () => ({}), status: () => ({}) },
      authService: {}, fetchJson: async () => ({ ok: true, status: 200, body: {} }), DIARY_BASE: 'http://diary.synthetic.invalid',
      checkAddress: async () => ({ ok: true }), servedModels: async (t) => { models.push(t.model); return [t.model]; },
    });
    await route({ method: 'POST' }, { setHeader() {} }, { path: '/api/embedding', authn: synthetic });
    reads[i].served_model = models.length ? models[0] : null;
  }
  const aggregates = [];
  for (const scope of ['all', 'projects', 'diary']) {
    for (const [p, d] of HALVES) {
      const r = await run(JSON.stringify({ action: 'clear-next', scope }), p, d);
      aggregates.push({ scope, projects_half: p, diary_half: d, status: r.status,
        projects: r.body.projects ?? null, diary: r.body.diary ?? null });
    }
  }
  const out = {
    version: 1,
    generator: 'crates/embedding-admin/tests/generate_fixtures.cjs (noevia-core server/routes/embedding.cjs)',
    sources: SOURCES,
    reads,
    aggregates,
  };
  process.stdout.write(JSON.stringify(out, null, 1) + '\n');
})().catch((err) => { process.stderr.write(String(err?.stack || err) + '\n'); process.exit(1); });
