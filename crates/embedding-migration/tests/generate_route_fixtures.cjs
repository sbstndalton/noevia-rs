#!/usr/bin/env node
'use strict';
// Differential fixtures for crates/embedding-migration's routes module, recorded from noevia-core's
// own server/routes/embedding.cjs (createEmbeddingRoutes), wholly synthetic.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/embedding-migration/tests/generate_route_fixtures.cjs > crates/embedding-migration/tests/fixtures/node-routes.v1.json
//
// Each case is one GET or POST by a synthetic administrator through the real route. The migration,
// the address check, the served-model probe, the Diary relay (fetchJson) and the embedding service
// (never managed here) are stubs answering from the case and recording what they were asked.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) { process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n'); process.exit(2); }
const SOURCES = {
  'server/routes/embedding.cjs': 'ae34b6be30f2b8880a5c6c0c141ed9d1e709e87ef92202bb3cc0e79465db9854',
  'server/embedding-served.cjs': '9ab1c0d06c60d53beef408a887c5ca4f090451e91457ae50f89ff7c029054f3b',
};
for (const [rel, want] of Object.entries(SOURCES)) {
  const got = crypto.createHash('sha256').update(fs.readFileSync(path.join(checkout, rel))).digest('hex');
  if (got !== want) { process.stderr.write(`source hash mismatch for ${rel}: ${got}\n`); process.exit(1); }
}
const { createEmbeddingRoutes } = require(path.join(path.resolve(checkout), 'server', 'routes', 'embedding.cjs'));

const LOCAL = 'http://10.0.0.5:8080', OTHER = 'http://10.0.0.6:8080', PUBLIC = 'http://203.0.113.7';
const OK = { ok: { qa: 'migration-status' } };
const DIARY_OK = { ok: true, status: 200, body: { qa: 'diary-ok' } };
const t = (o) => ({ model: 'qa-embed-b', ...o });
const fail = (status, code, extra = {}) => ({ throw: { message: `qa refused ${code || status}`, status, code, ...extra } });

// Each case: body (POST) or get: true; migration: answer per method; diary: answer per method; served per address; public: addresses refused.
const CASES = [
  { name: 'get', get: true },
  { name: 'get-diary-down', get: true, diary: { GET: { throw: 'connect ECONNREFUSED' } } },
  { name: 'get-no-diary', get: true, noDiary: true },
  { name: 'not-object', body: [] },
  { name: 'bad-action', body: { action: 'drop' } },
  { name: 'service-switch', body: { action: 'service-switch', model: 'qa-embed-b' } },
  { name: 'service-off', body: { action: 'service-off' } },
  { name: 'bad-scope', body: { action: 'finish', scope: 'all-of-it' } },
  { name: 'set-next-no-target', body: { action: 'set-next' } },
  { name: 'set-next-ok', body: { action: 'set-next', target: t({ profile: 'qa-p' }) } },
  { name: 'set-next-local', body: { action: 'set-next', target: t({ baseUrl: LOCAL, diaryBaseUrl: OTHER }) } },
  { name: 'set-next-public', body: { action: 'set-next', target: t({ baseUrl: PUBLIC }) } },
  { name: 'set-next-public-diary', body: { action: 'set-next', target: t({ baseUrl: LOCAL, diaryBaseUrl: ` ${PUBLIC} ` }) } },
  { name: 'set-next-public-diary-projects-scope', body: { action: 'set-next', scope: 'projects', target: t({ diaryBaseUrl: PUBLIC }) } },
  { name: 'set-next-blank-addresses', body: { action: 'set-next', target: t({ baseUrl: '  ', diaryBaseUrl: null }) } },
  { name: 'set-next-not-served', body: { action: 'set-next', target: t({}) }, served: { '': ['qa-embed-a'] } },
  { name: 'set-next-diary-not-served', body: { action: 'set-next', target: t({ baseUrl: LOCAL, diaryBaseUrl: OTHER }) }, served: { [LOCAL]: ['qa-embed-b'], [OTHER]: ['/x.gguf'] } },
  { name: 'set-next-unverified', body: { action: 'set-next', target: t({}) }, served: { '': null } },
  { name: 'set-next-diary-scope', body: { action: 'set-next', scope: 'diary', target: t({ diaryBaseUrl: OTHER }) } },
  { name: 'set-next-projects-scope', body: { action: 'set-next', scope: 'projects', target: t({}) } },
  { name: 'set-next-migration-refuses', body: { action: 'set-next', target: t({}) }, migration: { setNext: fail(409, 'alreadyActive') } },
  { name: 'set-next-migration-plain-error', body: { action: 'set-next', target: t({}) }, migration: { setNext: fail(undefined, undefined) } },
  { name: 'set-next-both-refuse', body: { action: 'set-next', target: t({}) }, migration: { setNext: fail(400, 'invalidTarget') }, diary: { POST: { ok: false, status: 409, body: { detail: 'qa diary busy', code: 'busy' } } } },
  { name: 'set-next-diary-refuses', body: { action: 'set-next', target: t({}) }, diary: { POST: { ok: false, status: 500, body: null } } },
  { name: 'set-next-diary-unreachable', body: { action: 'set-next', target: t({}) }, diary: { POST: { throw: 'x'.repeat(200) } } },
  { name: 'set-next-diary-error-object', body: { action: 'set-next', target: t({}) }, diary: { POST: { ok: false, status: 418, body: { error: { a: 1 }, code: 0 } } } },
  { name: 'set-next-array-target', body: { action: 'set-next', target: [] } },
  { name: 'set-next-numeric-model', body: { action: 'set-next', target: { model: 5, profile: 0, baseUrl: LOCAL } } },
  { name: 'clear-next', body: { action: 'clear-next' } },
  { name: 'clear-next-array-target', body: { action: 'clear-next', target: [1] } },
  { name: 'clear-next-target-sent', body: { action: 'clear-next', target: { profile: 'qa-p', baseUrl: LOCAL } } },
  { name: 'switch-force', body: { action: 'switch', force: true } },
  { name: 'switch-force-truthy', body: { action: 'switch', force: 1 } },
  { name: 'switch-not-ready', body: { action: 'switch' }, migration: { switchToNext: fail(409, 'notReady', { coverage: { ready: false } }) } },
  { name: 'switch-parity', body: { action: 'switch', scope: 'projects' }, migration: { switchToNext: fail(409, 'parityFailed', { parity: { ok: false, minCosine: 0.5 } }) } },
  { name: 'switch-served', body: { action: 'switch', scope: 'projects' }, migration: { switchToNext: fail(409, 'modelNotServed', { served: ['qa-x'] }) } },
  { name: 'rollback-both-fail', body: { action: 'rollback' }, migration: { rollback: fail(409, 'noPrevious') }, diary: { POST: { ok: false, status: 409, body: { error: 'qa none' } } } },
  { name: 'rollback-diary-only-fails', body: { action: 'rollback' }, diary: { POST: { ok: false, status: 409, body: { error: 'qa none' } } } },
  { name: 'finish-diary', body: { action: 'finish', scope: 'diary' } },
  { name: 'finish-long-error', body: { action: 'finish', scope: 'projects' }, migration: { finish: { throw: { message: 'y'.repeat(400), status: 999 } } } },
  { name: 'finish-no-diary', body: { action: 'finish' }, noDiary: true },
];

async function run(c) {
  const rec = { migration: [], diary: [], served: [], addresses: [], audit: [], sync: 0 };
  const answer = (spec, fallback) => {
    if (spec && spec.throw) { const e = Object.assign(Error(spec.throw.message), spec.throw); delete e.throw; throw e; }
    return spec && spec.ok ? spec.ok : fallback;
  };
  const m = c.migration || {};
  const migration = {
    status: () => ({ qa: 'status' }),
    setNext: async (target) => { rec.migration.push(['setNext', target ?? null]); return answer(m.setNext, OK.ok); },
    clearNext: async () => { rec.migration.push(['clearNext']); return answer(m.clearNext, OK.ok); },
    switchToNext: async (o) => { rec.migration.push(['switchToNext', o]); return answer(m.switchToNext, OK.ok); },
    rollback: async () => { rec.migration.push(['rollback']); return answer(m.rollback, OK.ok); },
    finish: async () => { rec.migration.push(['finish']); return answer(m.finish, OK.ok); },
  };
  const d = c.diary || {};
  const fetchJson = async (url, init) => {
    rec.diary.push([init.method, init.body ? JSON.parse(init.body) : null]);
    const v = d[init.method] || DIARY_OK;
    if (v.throw) throw Error(v.throw);
    return v;
  };
  const served = c.served || {};
  let status = null, body = null;
  const res = { setHeader() {} };
  const json = (_res, s, b) => { status = s; body = b; };
  const route = createEmbeddingRoutes({
    json, readJson: async () => c.body, migration, authService: { audit: (...a) => rec.audit.push(a) }, fetchJson,
    DIARY_BASE: c.noDiary ? '' : 'http://diary.invalid', checkAddress: async (a) => { rec.addresses.push(a); return a.includes('203.0.113.') ? { ok: false, reason: 'qa public address' } : { ok: true }; },
    servedModels: async (tg) => { rec.served.push([tg.baseUrl, tg.model]); const v = served[tg.baseUrl || '']; return v === undefined ? [tg.model] : v; },
    service: { managed: () => { rec.sync += 1; return false; }, refresh: async () => {}, status: () => null },
  });
  await route({ method: c.get ? 'GET' : 'POST' }, res, { path: '/api/embedding', authn: { user: { id: 'qa-admin', role: 'admin' } } });
  if (c.get) rec.sync = 0; // GET asks managed() for its own reason
  return { ...c, status, response: body, rec };
}

(async () => {
  const cases = [];
  for (const c of CASES) cases.push(await run(c));
  process.stdout.write(JSON.stringify({ version: 1, sources: SOURCES, cases }) + '\n');
})().catch((err) => { process.stderr.write(String(err?.stack || err) + '\n'); process.exit(1); });
