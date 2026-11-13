#!/usr/bin/env node
'use strict';
// F7 with an index: noevia-core's own rag.cjs filesContext over a project index Node wrote (indexProjectFile with
// deterministic stub embeddings), so its retrieval runs and excerpts reach the prompt. For each question it records
// the files given, the prompt text, onSources' placed and injected lists, the query vector Node bound and the index
// as filesContext's search read it. The Rust test runs the native search over the rebuilt index and the native
// filesContext over those hits, and must give Node's text and sources. Synthetic QA data; no model.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout with server/node_modules and the pinned server/wasm/dav-parse.wasm> \
//     node crates/project-retrieval/tests/generate_context_fixtures.cjs
//
// Writes crates/project-retrieval/tests/fixtures/F7-files-context-indexed.json.
process.env.NODE_ENV = 'test';
const crypto = require('crypto');
const fs = require('fs');
const os = require('os');
const path = require('path');
const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) { process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n'); process.exit(2); }
const srv = path.join(path.resolve(checkout), 'server');
const SOURCES = {
  'server/rag.cjs': '227d0b739858237554fe366269ef5926e76ac2275731f83f0fb19e3d31e6c024',
  'server/vector-slots.cjs': 'aa56665f88b9631179baac90ecfd180849e0e9f1b956ccc4ebc0e5bb4819c6be',
  'server/embedding-profile.cjs': 'fa349dd5909a912335bb81e81b1e670e1ed774cbca626da30853e24c3743b740',
  'server/quoted-name.cjs': '3e5aeb0c25b8d73f71ea7c7fb4f55a6db6cec9d1c6fa7fa03a7d6ea6cf044f86',
  'server/wasm/dav-parse.wasm': '2305d64d1a75a57b3c52eda673b384c819718aae540c570837b4aeb4ef5e5cb9',
};
for (const [rel, want] of Object.entries(SOURCES)) {
  const got = crypto.createHash('sha256').update(fs.readFileSync(path.join(checkout, rel))).digest('hex');
  if (got !== want) { process.stderr.write(`source hash mismatch for ${rel}: ${got}\n`); process.exit(1); }
}
for (const k of Object.keys(process.env)) if (/^(RERANK_|RAG_|NOEVIA_FEATURE_RAG|EMBEDDING_|FILES_CONTEXT)/.test(k)) delete process.env[k];
console.warn = () => {};
const rag = require(path.join(srv, 'rag.cjs'));
const Database = require(path.join(srv, 'node_modules', 'better-sqlite3'));
const sqliteVec = require(path.join(srv, 'node_modules', 'sqlite-vec'));

const VOCAB = ['alpha', 'bravo', 'charlie', 'delta', 'echo', 'foxtrot', 'golf', 'hotel'];
const DIM = 9;
const vectorFor = (text) => {
  const v = new Array(DIM).fill(0);
  for (const w of String(text).toLowerCase().match(/[a-z]+/g) || []) { const i = VOCAB.indexOf(w); if (i >= 0) v[i] += 1; }
  v[DIM - 1] += crypto.createHash('sha256').update(String(text)).digest()[0] / 2550;
  return v.map((x) => Math.fround(x));
};
let lastQuery = null;
rag._test.setRagFetch(async (_url, init) => {
  const body = JSON.parse(init.body);
  const data = body.input.map((t, index) => ({ index, embedding: vectorFor(t) }));
  if (body.input.length === 1 && String(body.input[0]).startsWith('task: search result | query: ')) lastQuery = data[0].embedding;
  return new Response(JSON.stringify({ data }), { status: 200 });
});

const words = (n, seed, bias) => Array.from({ length: n }, (_, i) => (i % 2 === 0 && bias ? bias : VOCAB[(i * 3 + seed) % VOCAB.length])).join(' ');
const prose = (sentences, seed, bias) => Array.from({ length: sentences }, (_, s) => words(9 + (s % 5), s + seed, bias) + '.').join(' ');

function dump(file) {
  const db = new Database(file, { readonly: true });
  sqliteVec.load(db);
  const master = db.prepare('SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY rowid').all();
  const virt = master.filter((m) => /CREATE VIRTUAL TABLE/i.test(m.sql)).map((m) => m.name);
  const out = {
    schema: master.filter((m) => !virt.some((v) => m.name !== v && m.name.startsWith(`${v}_`)) && !m.name.startsWith('sqlite_')).map((m) => m.sql),
    chunks: db.prepare('SELECT id, file, chunk_no, body, content_hash, version, direct, kind, meta FROM chunks ORDER BY id').all(),
    vec_meta: db.prepare('SELECT key, value FROM vec_meta ORDER BY rowid').all(),
    vectors: Object.fromEntries(virt.map((v) => [v, db.prepare(`SELECT chunk_id, embedding FROM ${v}`).all().map((r) => ({ chunk_id: r.chunk_id, embedding: Array.from(new Float32Array(r.embedding.buffer, r.embedding.byteOffset, r.embedding.length / 4)) }))])),
    maps: Object.fromEntries(master.filter((x) => x.type === 'table' && /^vecmap_[0-9a-f]{16}$/.test(x.name)).map((m) => [m.name, db.prepare(`SELECT chunk_id, content_hash FROM ${m.name} ORDER BY rowid`).all()])),
  };
  db.close();
  return out;
}

(async () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-r4-context-'));
  const USER = 'qa-user-r4';
  const target = { model: 'qa-embed-ctx', profile: 'embeddinggemma-2', baseUrl: 'http://127.0.0.1:9' };
  rag.init({ dataDir: dir, inferenceUrl: 'http://127.0.0.1:9/v1', userDataDirFn: (u) => path.join(dir, 'users', u), userActive: () => true, embeddingConfig: { active: () => ({ ...target }) } });
  const small = 'A short QA note about golf and hotel.';
  const longA = prose(70, 1, 'alpha');
  const longB = prose(70, 4, 'hotel');
  const extra = prose(60, 2, 'alpha');
  const notIndexed = prose(50, 6, 'echo');
  for (const [name, text] of [['qa-small.md', small], ['qa-long-a.md', longA], ['qa-long-b.md', longB], ['qa-extra.md', extra]]) await rag.indexProjectFile('qa-proj-ctx', name, text, USER);
  const file = path.join(dir, 'users', USER, 'rag', 'qa-proj-ctx.db');
  const index = dump(file);
  const sets = {
    project: [{ name: 'qa-small.md', content: small }, { name: 'qa-long-a.md', content: longA }, { name: 'qa-long-b.md', content: longB }, { name: 'qa-unindexed.md', content: notIndexed }],
    attached: [{ name: 'qa-small.md', content: small, attachment: { reason: 'QA reason: partly read' } }, { name: 'qa-long-b.md', content: longB }],
  };
  const cases = [];
  for (const [set, query, attached] of [
    ['project', 'alpha bravo', false],
    ['project', 'what does qa-small.md say about hotel', false],
    ['project', 'zulu nothing matches', false],
    ['project', 'alpha from qa-extra.md which is not attached', false],
    ['attached', 'hotel golf', true],
  ]) {
    lastQuery = null;
    let sources = null;
    const text = await rag.filesContext('qa-proj-ctx', sets[set], query, USER, (placed, injected) => { sources = { placed, injected }; }, { attached });
    cases.push({ set, query, attached, query_vector: lastQuery, text, ...sources });
  }
  fs.rmSync(dir, { recursive: true, force: true });
  const out = path.join(__dirname, 'fixtures', 'F7-files-context-indexed.json');
  fs.writeFileSync(out, JSON.stringify({ sources: SOURCES, source: 'server/rag.cjs filesContext :1264-1335 with searchProject :878-922 over an index Node wrote', target, user: USER, project: 'qa-proj-ctx', index, files: sets, cases }, null, 1) + '\n');
  process.stdout.write(`wrote ${out}: ${cases.length} cases\n`);
})().catch((e) => { process.stderr.write(String(e.stack || e) + '\n'); process.exit(1); });
