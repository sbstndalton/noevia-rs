#!/usr/bin/env node
'use strict';
// F15 of handoff/foundation/R4.md: the optional reranker of project search, recorded from noevia-core's own code on
// synthetic inputs: rag.cjs rerankPool and its backend (rag.cjs rerankBackend -> decision/backends.cjs
// llamaRerankBackend / teiRerankBackend through the rag fetch seam) under decision/index.cjs createDecisions (its
// request/result checks run in the pinned dav-parse wasm). A scripted reranker answers each request; every HTTP
// request the backend sent and the pool rerankPool returned are recorded. No model, no network.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout with server/node_modules and server/wasm/dav-parse.wasm (the build
//     pinned by its server/dav-parse.lock)> node crates/project-retrieval/tests/generate_rerank_fixtures.cjs
//
// Writes crates/project-retrieval/tests/fixtures/F15-rerank.json.
process.env.NODE_ENV = 'test';
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) { process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n'); process.exit(2); }
const srv = path.join(path.resolve(checkout), 'server');
const SOURCES = {
  'server/rag.cjs': '227d0b739858237554fe366269ef5926e76ac2275731f83f0fb19e3d31e6c024',
  'server/decision/index.cjs': 'b0c917d0c2f7779b6979c8e171911665434bdf61b0a7b07e88115999f751587b',
  'server/decision/backends.cjs': '68b1a3d0839725930ffe615053341016e306131d2986bbef5c74c73109afc56f',
  'server/wasm/dav-parse.wasm': '2305d64d1a75a57b3c52eda673b384c819718aae540c570837b4aeb4ef5e5cb9',
};
for (const [rel, want] of Object.entries(SOURCES)) {
  const got = crypto.createHash('sha256').update(fs.readFileSync(path.join(checkout, rel))).digest('hex');
  if (got !== want) { process.stderr.write(`source hash mismatch for ${rel}: ${got}\n`); process.exit(1); }
}
console.warn = () => {};
const rag = require(path.join(srv, 'rag.cjs'));
const { createDecisions } = require(path.join(srv, 'decision', 'index.cjs'));

// The scripted reranker: `script(url, body)` answers { status, body } or 'hang' (never answers).
let script = () => ({ status: 500, body: {} });
let seen = [];
rag._test.setRagFetch(async (url, init) => {
  const body = JSON.parse(init.body);
  // The texts sent, as UTF-16 code units: a label cut inside a surrogate pair holds a lone surrogate, which JSON
  // readers outside JS refuse.
  const units = (t) => Array.from({ length: t.length }, (_, i) => t.charCodeAt(i));
  const sent = { ...body, ...(body.documents ? { documents: body.documents.map(units) } : {}), ...(body.texts ? { texts: body.texts.map(units) } : {}) };
  // `raw`: the body text exactly as sent (JSON.stringify writes a lone surrogate as a \udXXX escape, so it is plain JSON).
  seen.push({ url, method: init.method, redirect: init.redirect ?? null, authorization: init.headers?.authorization ?? null, contentType: init.headers?.['content-type'] ?? null, body: sent, raw: init.body });
  const answer = script(url, body);
  if (answer === 'hang') return new Promise(() => {});
  return new Response(typeof answer.body === 'string' ? answer.body : JSON.stringify(answer.body), { status: answer.status });
});

const pool = (n, seed = 0) => Array.from({ length: n }, (_, i) => ({ file: `qa-${(i + seed) % 3}.md`, body: `qa chunk ${i} ${'alpha bravo '.repeat(i % 4)}`.trim(), score: Math.round((0.9 - i * 0.03) * 1000) / 1000 }));
const llamaScores = (docs, f) => ({ status: 200, body: { model: 'qa-rerank-served', results: docs.map((_, index) => ({ index, relevance_score: f(index) })) } });
const teiScores = (docs, f) => ({ status: 200, body: docs.map((_, index) => ({ index, score: f(index) })) });
const long = 'x'.repeat(3999) + '😀' + 'y'.repeat(10);

const cases = [
  // llama.cpp /v1/rerank
  { name: 'llama reverses the pool', backend: 'llamacpp', model: null, apiKey: null, n: 12, keep: 6, respond: (u, b) => llamaScores(b.documents, (i) => i) },
  { name: 'llama with model and key, keep 3', backend: 'llamacpp', model: 'qa-rerank-model', apiKey: 'qa-key', n: 8, keep: 3, respond: (u, b) => llamaScores(b.documents, (i) => (i % 3) - i / 100) },
  { name: 'llama ties broken by id text (10 before 2)', backend: 'llamacpp', model: null, apiKey: null, n: 12, keep: 12, respond: (u, b) => llamaScores(b.documents, (i) => (i === 2 || i === 10 ? 5 : 0)) },
  { name: 'llama rows out of order, string index', backend: 'llamacpp', model: null, apiKey: null, n: 4, keep: 4, respond: () => ({ status: 200, body: { results: [{ index: 3, relevance_score: 1 }, { index: '1', relevance_score: 4 }, { index: 0, relevance_score: 2 }, { index: 2, relevance_score: 3 }] } }) },
  { name: 'llama partial result falls back to cosine', backend: 'llamacpp', model: null, apiKey: null, n: 6, keep: 6, respond: (u, b) => ({ status: 200, body: { results: b.documents.slice(1).map((_, i) => ({ index: i + 1, relevance_score: i })) } }) },
  { name: 'llama duplicate index is partial', backend: 'llamacpp', model: null, apiKey: null, n: 3, keep: 3, respond: () => ({ status: 200, body: { results: [{ index: 0, relevance_score: 1 }, { index: 0, relevance_score: 2 }, { index: 1, relevance_score: 3 }] } }) },
  { name: 'llama extra out-of-range rows are ignored', backend: 'llamacpp', model: null, apiKey: null, n: 3, keep: 3, respond: () => ({ status: 200, body: { results: [{ index: 0, relevance_score: 1 }, { index: 1, relevance_score: 3 }, { index: 2, relevance_score: 2 }, { index: 7, relevance_score: 9 }, { index: 1.5, relevance_score: 9 }, { index: -1, relevance_score: 9 }] } }) },
  { name: 'llama non-finite score is partial', backend: 'llamacpp', model: null, apiKey: null, n: 2, keep: 2, respond: () => ({ status: 200, body: { results: [{ index: 0, relevance_score: '1' }, { index: 1, relevance_score: 2 }] } }) },
  { name: 'llama results missing', backend: 'llamacpp', model: null, apiKey: null, n: 2, keep: 2, respond: () => ({ status: 200, body: { data: [] } }) },
  { name: 'llama 500 falls back', backend: 'llamacpp', model: null, apiKey: null, n: 5, keep: 2, respond: () => ({ status: 500, body: { error: 'qa' } }) },
  { name: 'llama body not JSON falls back', backend: 'llamacpp', model: null, apiKey: null, n: 3, keep: 3, respond: () => ({ status: 200, body: 'not json' }) },
  { name: 'llama label cut at 4000 units inside a surrogate pair', backend: 'llamacpp', model: null, apiKey: null, n: 2, keep: 2, labels: [long, 'short'], respond: (u, b) => llamaScores(b.documents, (i) => i) },
  // TEI /rerank
  { name: 'tei reverses', backend: 'tei', model: 'qa-tei-model', apiKey: null, n: 7, keep: 6, respond: (u, b) => teiScores(b.texts, (i) => i / 10) },
  { name: 'tei with key, keep 2', backend: 'tei', model: null, apiKey: 'qa-key', n: 5, keep: 2, respond: (u, b) => teiScores(b.texts, (i) => -i) },
  { name: 'tei object body is malformed', backend: 'tei', model: null, apiKey: null, n: 3, keep: 3, respond: () => ({ status: 200, body: { results: [] } }) },
  { name: 'tei too few rows', backend: 'tei', model: null, apiKey: null, n: 3, keep: 3, respond: () => ({ status: 200, body: [{ index: 0, score: 1 }, { index: 1, score: 2 }] }) },
  { name: 'tei fractional index', backend: 'tei', model: null, apiKey: null, n: 2, keep: 2, respond: () => ({ status: 200, body: [{ index: 0, score: 1 }, { index: 0.5, score: 2 }] }) },
  { name: 'tei string index', backend: 'tei', model: null, apiKey: null, n: 2, keep: 2, respond: () => ({ status: 200, body: [{ index: 0, score: 1 }, { index: '1', score: 2 }] }) },
  { name: 'tei repeated index', backend: 'tei', model: null, apiKey: null, n: 2, keep: 2, respond: () => ({ status: 200, body: [{ index: 0, score: 1 }, { index: 0, score: 2 }] }) },
  { name: 'tei index out of range', backend: 'tei', model: null, apiKey: null, n: 2, keep: 2, respond: () => ({ status: 200, body: [{ index: 0, score: 1 }, { index: 2, score: 2 }] }) },
  { name: 'tei null score', backend: 'tei', model: null, apiKey: null, n: 2, keep: 2, respond: () => ({ status: 200, body: [{ index: 0, score: 1 }, { index: 1, score: null }] }) },
  { name: 'tei negative zero index', backend: 'tei', model: null, apiKey: null, n: 2, keep: 2, respond: () => ({ status: 200, body: '[{"index":-0,"score":1},{"index":1,"score":2}]' }) },
  { name: 'tei 503 falls back', backend: 'tei', model: null, apiKey: null, n: 4, keep: 4, respond: () => ({ status: 503, body: {} }) },
  // Pool shapes
  { name: 'a pool of one is not sent', backend: 'llamacpp', model: null, apiKey: null, n: 1, keep: 6, respond: (u, b) => llamaScores(b.documents, (i) => i) },
  { name: 'an empty pool is not sent', backend: 'tei', model: null, apiKey: null, n: 0, keep: 6, respond: (u, b) => teiScores(b.texts, (i) => i) },
  { name: 'a pool of three keeps three', backend: 'tei', model: null, apiKey: null, n: 3, keep: 6, respond: (u, b) => teiScores(b.texts, (i) => i) },
];

(async () => {
  const out = [];
  for (const c of cases) {
    const items = pool(c.n).map((h, i) => (c.labels ? { ...h, body: c.labels[i] } : h));
    const backend = rag._test.rerankBackend('http://qa-rerank:8080/v1/', c.backend, c.model, c.apiKey);
    const decisions = createDecisions({ backends: { [backend.id]: backend }, chains: { 'rag.rerank': [backend.id] } });
    seen = [];
    script = c.respond;
    const result = await rag.rerankPool({ query: 'qa question about alpha', pool: items, decisions, keep: c.keep, deadlineMs: 3000 });
    out.push({ name: c.name, backend: c.backend, baseUrl: 'http://qa-rerank:8080/v1/', model: c.model, apiKey: c.apiKey, keep: c.keep, deadlineMs: 3000, query: 'qa question about alpha', pool: items, requests: seen, answer: c.respond === undefined ? null : (seen.length ? c.respond(seen[0].url, seen[0].body) : null), result });
  }
  // The deadline: a reranker that never answers falls back at deadlineMs; three misses in a row bench it for 60 s,
  // so the fourth search sends nothing.
  {
    const backend = rag._test.rerankBackend('http://qa-rerank:8080', 'tei', null, null);
    const decisions = createDecisions({ backends: { [backend.id]: backend }, chains: { 'rag.rerank': [backend.id] } });
    script = () => 'hang';
    const runs = [];
    for (let i = 0; i < 4; i++) {
      seen = [];
      const items = pool(4);
      const result = await rag.rerankPool({ query: 'qa slow', pool: items, decisions, keep: 6, deadlineMs: 100 });
      runs.push({ sent: seen.length, result });
    }
    out.push({ name: 'deadline misses, then the bench', backend: 'tei', baseUrl: 'http://qa-rerank:8080', model: null, apiKey: null, keep: 6, deadlineMs: 100, query: 'qa slow', pool: pool(4), runs });
  }
  const file = path.join(__dirname, 'fixtures', 'F15-rerank.json');
  fs.writeFileSync(file, JSON.stringify({ sources: SOURCES, source: 'server/rag.cjs rerankPool :151-165, rerankBackend :45-54; server/decision/index.cjs createDecisions decide/withDeadline (bench after 3 deadline misses for 60 s); server/decision/backends.cjs llamaRerankBackend, teiRerankBackend, order', cases: out }, null, 1) + '\n');
  process.stdout.write(`wrote ${file}: ${out.length} cases\n`);
  process.exit(0);
})().catch((e) => { process.stderr.write(String(e.stack || e) + '\n'); process.exit(1); });
