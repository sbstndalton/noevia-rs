#!/usr/bin/env node
'use strict';
// The two decisions a native search must make before it embeds a query, recorded from noevia-core's own code on
// synthetic inputs (no network, no model):
// - rerank-target.cjs rerankTarget(env): whether the reranker is on, where, and why not;
// - rag.cjs embeddingOff(target) over embedding-service.cjs in native-owner mode (the native embedding worker's
//   status snapshot is the service state Node reads).
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> node crates/project-retrieval/tests/generate_policy_fixtures.cjs
//
// - rust-embedding-background.cjs createOwner().snapshot(): which status files Node accepts as current.
//
// Writes crates/project-retrieval/tests/fixtures/policy-rerank-target.json, policy-embedding-off.json and
// policy-snapshot.json.
process.env.NODE_ENV = 'test';
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) { process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n'); process.exit(2); }
const srv = path.join(path.resolve(checkout), 'server');
const SOURCES = {
  'server/rag.cjs': '227d0b739858237554fe366269ef5926e76ac2275731f83f0fb19e3d31e6c024',
  'server/rerank-target.cjs': '66f4a7f2b60e54737b0af78683ed74dcb2ff1e2e0fce762cf3bc5940bc192084',
  'server/embedding-service.cjs': '1266640492aea8841d73d25ec22dba1fe2daa962c04dd415df81c7dfed4468dd',
  'server/rust-embedding-background.cjs': 'c45fc4a2402a8bb2c8825ef26f3ab978425c099bfdd1f7f7cf2bb72afd28df2c',
};
for (const [rel, want] of Object.entries(SOURCES)) {
  const got = crypto.createHash('sha256').update(fs.readFileSync(path.join(checkout, rel))).digest('hex');
  if (got !== want) { process.stderr.write(`source hash mismatch for ${rel}: ${got}\n`); process.exit(1); }
}
const outDir = path.join(__dirname, 'fixtures');
const write = (name, obj) => fs.writeFileSync(path.join(outDir, name), JSON.stringify({ sources: SOURCES, ...obj }, null, 1) + '\n');

// ── rerankTarget ──────────────────────────────────────────────────────────────────────────────────
const { rerankTarget } = require(path.join(srv, 'rerank-target.cjs'));
const on = { NOEVIA_FEATURE_RAG_RERANK: '1' };
const envs = [
  {},
  { RERANK_BASE_URL: 'http://qa-rerank:8080' },
  { ...on },
  { ...on, RERANK_BASE_URL: 'http://qa-rerank:8080' },
  { ...on, RERANK_BASE_URL: '  http://qa-rerank:8080  ', RERANK_MODEL: ' qa-rerank-model ' },
  { ...on, RERANK_BACKEND: 'tei' },
  { ...on, RERANK_BACKEND: ' TEI ', RERANK_BASE_URL: 'http://10.0.0.9:8080' },
  { ...on, RERANK_BACKEND: 'llama.cpp', RERANK_BASE_URL: 'http://qa-rerank:8080' },
  { ...on, RERANK_BACKEND: 'llama', RERANK_BASE_URL: 'http://qa-rerank:8080' },
  { ...on, RERANK_BACKEND: 'other', RERANK_BASE_URL: 'http://qa-rerank:8080' },
  { ...on, RERANK_BACKEND: 'toString', RERANK_BASE_URL: 'http://qa-rerank:8080' },
  { ...on, RERANK_BACKEND: '__proto__', RERANK_BASE_URL: 'http://qa-rerank:8080' },
  { NOEVIA_FEATURE_RAG_RERANK: 'true', RERANK_BASE_URL: 'http://qa-rerank:8080' },
  { NOEVIA_FEATURE_RAG_RERANK: ' ON ', RERANK_BASE_URL: 'http://qa-rerank:8080' },
  { NOEVIA_FEATURE_RAG_RERANK: 'yes', RERANK_BASE_URL: 'http://qa-rerank:8080' },
  { NOEVIA_FEATURE_RAG_RERANK: '0', RERANK_BASE_URL: 'http://qa-rerank:8080' },
  { NOEVIA_FEATURE_RAG_RERANK: 'false', RERANK_BASE_URL: 'http://qa-rerank:8080' },
  { ...on, RERANK_BASE_URL: 'http://qa-engine:11434/v1', INFERENCE_BASE_URL: 'http://QA-ENGINE:11434' },
  { ...on, RERANK_BASE_URL: 'http://qa-engine:11434', INFERENCE_BASE_URL: 'http://qa-engine:11434/v1', RERANK_SHARED_ENGINE: ' Allow ' },
  { ...on, RERANK_BASE_URL: 'http://qa-engine:11434', MODEL_MANAGER_BASE_URL: 'http://qa-engine:11434' },
  { ...on, RERANK_BASE_URL: 'http://qa-engine:11434', LEMONADE_BASE_URL: 'http://qa-engine:11434/api' },
  { ...on, RERANK_BASE_URL: 'http://qa-engine:11435', INFERENCE_BASE_URL: 'http://qa-engine:11434' },
  { ...on, RERANK_BASE_URL: 'http://qa-engine', INFERENCE_BASE_URL: 'http://qa-engine:80' },
  { ...on, RERANK_BASE_URL: 'https://qa-engine', INFERENCE_BASE_URL: 'https://qa-engine:443/x' },
  { ...on, RERANK_BASE_URL: 'not a url', INFERENCE_BASE_URL: 'not a url' },
  { ...on, RERANK_BASE_URL: 'http://qa-engine:11434', INFERENCE_BASE_URL: 'garbage' },
  { ...on, RERANK_BACKEND: 'tei', INFERENCE_BASE_URL: 'http://rerank:8080' },
  { ...on, RERANK_BASE_URL: 'http://[::1]:8080', INFERENCE_BASE_URL: 'http://[0:0:0:0:0:0:0:1]:8080' },
  { ...on, RERANK_BASE_URL: 'http://Qa-Rerank.example:8080/', INFERENCE_BASE_URL: 'http://qa-rerank.example:8080/v1' },
  { ...on, RERANK_BASE_URL: 'http://user:pw@qa-engine:1', INFERENCE_BASE_URL: 'http://qa-engine:1' },
];
write('policy-rerank-target.json', {
  source: 'server/rerank-target.cjs rerankTarget :44-60, engineOrigins :29, origin :21',
  cases: envs.map((env) => ({ env, out: rerankTarget(env) })),
});

// ── embeddingOff ──────────────────────────────────────────────────────────────────────────────────
const rag = require(path.join(srv, 'rag.cjs'));
const embeddingService = require(path.join(srv, 'embedding-service.cjs'));
const NOW = 1_800_000_000_000;
const svc = (extra) => ({ managed: true, models: [{ model: 'qa-embed' }], ...extra });
const snapshots = [
  ['no snapshot', null],
  ['snapshot without service', { lastPass: {} }],
  ['enabled', { service: svc({ enabled: true }), serviceAt: NOW - 1000 }],
  ['disabled', { service: svc({ enabled: false }), serviceAt: NOW - 1000 }],
  ['enabled is the string "true"', { service: svc({ enabled: 'true' }), serviceAt: NOW - 1000 }],
  ['enabled missing', { service: svc({}), serviceAt: NOW - 1000 }],
  ['enabled but stale by age', { service: svc({ enabled: true }), serviceAt: NOW - 15001 }],
  ['enabled, exactly 15 s old', { service: svc({ enabled: true }), serviceAt: NOW - 15000 }],
  ['enabled but marked stale', { service: svc({ enabled: true, stale: true }), serviceAt: NOW - 1000 }],
  ['disabled, observed in the future', { service: svc({ enabled: false }), serviceAt: NOW + 1 }],
  ['disabled, observed now', { service: svc({ enabled: false }), serviceAt: NOW }],
  ['disabled, serviceAt a string', { service: svc({ enabled: false }), serviceAt: String(NOW - 1000) }],
  ['disabled, serviceAt missing', { service: svc({ enabled: false }) }],
  ['disabled, service null', { service: null, serviceAt: NOW - 1000 }],
  ['disabled, service false', { service: false, serviceAt: NOW - 1000 }],
  ['disabled, service an empty object', { service: {}, serviceAt: NOW - 1000 }],
];
const serviceUrls = [null, 'http://qa-embed:8080', 'http://qa-embed:8080/v1/', '  http://qa-embed:8080//  '];
const targets = [
  { model: 'qa-embed', profile: 'embeddinggemma-2' },
  { model: 'qa-embed', profile: 'embeddinggemma-2', baseUrl: 'http://qa-embed:8080' },
  { model: 'qa-embed', profile: 'embeddinggemma-2', baseUrl: 'http://qa-embed:8080/v1' },
  { model: 'qa-embed', profile: 'embeddinggemma-2', baseUrl: 'http://qa-embed:8080/' },
  { model: 'qa-embed', profile: 'embeddinggemma-2', baseUrl: 'http://QA-EMBED:8080' },
  { model: 'qa-embed', profile: 'embeddinggemma-2', baseUrl: 'http://qa-other:8080' },
  { model: 'qa-embed', profile: 'embeddinggemma-2', baseUrl: '' },
];
const cases = [];
for (const [name, snapshot] of snapshots) {
  for (const serviceUrl of serviceUrls) {
    const env = serviceUrl === null ? {} : { EMBEDDING_SERVICE_URL: serviceUrl };
    for (const target of targets) {
      // A fresh service per case: Node keeps the last good observation per process; one observation here.
      embeddingService.install(embeddingService.createEmbeddingService({ nativeOwner: { snapshot: () => snapshot }, env, now: () => NOW, log: () => {} }));
      cases.push({ name, snapshot, env, target, off: rag.embeddingOff(target) });
    }
  }
}
embeddingService.install(null);
cases.push({ name: 'no service installed', snapshot: null, env: {}, target: targets[1], off: rag.embeddingOff(targets[1]), installed: false });
write('policy-embedding-off.json', {
  source: 'server/rag.cjs embeddingOff :289-294; server/embedding-service.cjs createEmbeddingService observeNative/status/enabled/isAt/managed (native owner), sameAddress :6-9, STALE_MS',
  now: NOW,
  cases,
});
// ── snapshot() ────────────────────────────────────────────────────────────────────────────────────
const os = require('os');
const { createOwner } = require(path.join(srv, 'rust-embedding-background.cjs'));
const START = 'qa-start-1';
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-r4-snapshot-'));
const owner = createOwner({ verifyPair: false, dataDir: dir, now: () => NOW, env: { NOEVIA_START_ID: START } });
const base = { protocol: 'rust-embedding-worker-v1', start: START, at: NOW - 1000, service: { managed: true, enabled: true }, serviceAt: NOW - 2000 };
const files = [
  ['current', JSON.stringify(base)],
  ['15 s old', JSON.stringify({ ...base, at: NOW - 15000 })],
  ['15.001 s old', JSON.stringify({ ...base, at: NOW - 15001 })],
  ['written in the future', JSON.stringify({ ...base, at: NOW + 1 })],
  ['at now', JSON.stringify({ ...base, at: NOW })],
  ['another start', JSON.stringify({ ...base, start: 'qa-start-0' })],
  ['another protocol', JSON.stringify({ ...base, protocol: 'rust-embedding-worker-v2' })],
  ['at a string', JSON.stringify({ ...base, at: String(NOW - 1000) })],
  ['at missing', JSON.stringify({ ...base, at: undefined })],
  ['not JSON', '{"protocol":'],
  ['JSON null', 'null'],
  ['missing file', null],
];
const snapshotCases = files.map(([name, text]) => {
  const file = path.join(dir, 'embedding-worker-status.json');
  fs.rmSync(file, { force: true });
  if (text !== null) fs.writeFileSync(file, text);
  return { name, text, start: START, accepted: owner.snapshot() !== null };
});
fs.rmSync(dir, { recursive: true, force: true });
write('policy-snapshot.json', { source: 'server/rust-embedding-background.cjs createOwner snapshot() :30-35', now: NOW, cases: snapshotCases });

process.stdout.write(`wrote ${envs.length} rerank-target and ${cases.length} embedding-off cases\n`);
