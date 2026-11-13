#!/usr/bin/env node
'use strict';
// Search fixtures (F2-F6 of handoff/foundation/R4.md) for crates/project-retrieval, recorded from noevia-core's own
// server/rag.cjs and server/vector-slots.cjs with better-sqlite3 + sqlite-vec 0.1.9, on synthetic QA-named projects.
// No model and no network: the embeddings endpoint is the test seam (_test.setRagFetch) answering with deterministic
// word-count vectors.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout with server/node_modules installed> \
//     node crates/project-retrieval/tests/generate_search_fixtures.cjs
//
// Writes crates/project-retrieval/tests/fixtures/F2-F6-search.json. For every scenario it records what Node did
// (steps), the index as Node left it (schema SQL, chunks, vec_meta, every slot's vectors and hash map) and, per
// search, the query vector Node bound into its SQL, Node's in-process wanted-version state for the project's files,
// and searchProject's answer. The Rust test rebuilds the index from the dump and must give the same answers.
// The source files' sha256 are checked: a changed Node file needs a fresh recording.
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
};
for (const [rel, want] of Object.entries(SOURCES)) {
  const got = crypto.createHash('sha256').update(fs.readFileSync(path.join(checkout, rel))).digest('hex');
  if (got !== want) { process.stderr.write(`source hash mismatch for ${rel}: ${got}\n`); process.exit(1); }
}
// The reranker stays off: none of its variables are set (rerank-target.cjs), so the pool is TOP_K = 6.
for (const k of Object.keys(process.env)) if (/^(RERANK_|RAG_|NOEVIA_FEATURE_RAG|EMBEDDING_|FILES_CONTEXT)/.test(k)) delete process.env[k];
console.warn = () => {}; // rag.cjs logs expected failures (an index made unavailable on purpose)

const rag = require(path.join(srv, 'rag.cjs'));
const slots = require(path.join(srv, 'vector-slots.cjs'));
const Database = require(path.join(srv, 'node_modules', 'better-sqlite3'));
const sqliteVec = require(path.join(srv, 'node_modules', 'sqlite-vec'));

// Deterministic vectors: one dimension per vocabulary word (its count in the framed text), and "zulu" pulls every
// dimension down, so some pairs score below MIN_SCORE or negative. Identical texts give identical vectors (ties).
const VOCAB = ['alpha', 'bravo', 'charlie', 'delta', 'echo', 'foxtrot', 'golf', 'hotel'];
function vectorFor(text, dim) {
  const words = String(text).toLowerCase().match(/[a-z]+/g) || [];
  const v = new Array(dim).fill(0);
  for (const w of words) {
    const i = VOCAB.indexOf(w);
    if (i >= 0 && i < dim) v[i] += 1;
    if (w === 'zulu') for (let k = 0; k < dim; k++) v[k] -= 0.5;
  }
  // A small per-text offset so different texts rarely tie by accident; identical texts still tie.
  const h = crypto.createHash('sha256').update(String(text)).digest();
  v[dim - 1] += h[0] / 2550;
  return v.map((x) => Math.fround(x));
}
let dim = 8;
let wrongWidthFor = null; // a query text answered with a vector one wider than the slot
let lastQuery = null;
const calls = [];
rag._test.setRagFetch(async (url, init) => {
  const body = JSON.parse(init.body);
  const data = body.input.map((t, index) => {
    const d = wrongWidthFor && t.includes(wrongWidthFor) ? dim + 1 : dim;
    const embedding = vectorFor(t, d);
    if (t.startsWith('task: search result | query: ') || body.input.length === 1 && !t.startsWith('title: ')) lastQuery = { framed: t, vector: embedding };
    return { index, embedding };
  });
  calls.push({ model: body.model, inputs: body.input.length });
  return new Response(JSON.stringify({ data }), { status: 200, headers: { 'Content-Type': 'application/json' } });
});

const words = VOCAB;
// Synthetic prose: sentences of vocabulary words, `bias` repeated words weight one dimension.
function prose(sentences, seed, bias = null, biasEvery = 0) {
  const out = [];
  for (let s = 0; s < sentences; s++) {
    const n = 8 + ((s * 5 + seed) % 7);
    const ws = [];
    for (let i = 0; i < n; i++) ws.push(bias && biasEvery && (i + s) % biasEvery === 0 ? bias : words[(i * 3 + s * 7 + seed) % words.length]);
    out.push(ws.join(' ') + '.');
    if (s % 6 === 5) out.push('\n\n');
  }
  return out.join(' ').replace(/ \n\n /g, '\n\n');
}

const sourcesSha = SOURCES;
const scenarios = [];
const snapshots = {}; // index dumps by content hash, shared by the searches that read them
const tmpRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-r4-search-'));

function dumpIndex(file) {
  const db = new Database(file, { readonly: true });
  sqliteVec.load(db);
  const master = db.prepare("SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY rowid").all();
  const virtualNames = master.filter((m) => /CREATE VIRTUAL TABLE/i.test(m.sql)).map((m) => m.name);
  // vec0 shadow tables and sqlite's own are recreated by the virtual table; everything else is recorded.
  const schema = master.filter((m) => !virtualNames.some((v) => m.name !== v && m.name.startsWith(`${v}_`)) && !m.name.startsWith('sqlite_')).map((m) => m.sql);
  const chunks = db.prepare('SELECT id, file, chunk_no, body, content_hash, version, direct, kind, meta FROM chunks ORDER BY id').all();
  const vecMeta = db.prepare('SELECT key, value FROM vec_meta ORDER BY rowid').all();
  const vectors = {};
  for (const v of virtualNames) {
    vectors[v] = db.prepare(`SELECT chunk_id, embedding FROM ${v}`).all()
      .map((r) => ({ chunk_id: r.chunk_id, embedding: Array.from(new Float32Array(r.embedding.buffer, r.embedding.byteOffset, r.embedding.length / 4)) }));
  }
  const maps = {};
  for (const m of master.filter((x) => x.type === 'table' && /^vecmap_[0-9a-f]{16}$/.test(x.name))) {
    maps[m.name] = db.prepare(`SELECT chunk_id, content_hash FROM ${m.name} ORDER BY rowid`).all();
  }
  db.close();
  return { schema, chunks, vec_meta: vecMeta, vectors, maps };
}


function scenario(name, target, build) {
  const dataDir = path.join(tmpRoot, name);
  fs.mkdirSync(dataDir, { recursive: true });
  let active = true;
  const users = new Set(['qa-user-r4']);
  let current = target;
  const start = () => rag.init({
    dataDir,
    inferenceUrl: 'http://127.0.0.1:9/v1',
    userDataDirFn: (u) => path.join(dataDir, 'users', u),
    userActive: (u) => active && users.has(u),
    embeddingConfig: { active: () => ({ ...current }) },
  });
  start();
  const rec = { name, target, steps: [], searches: [] };
  const ctx = {
    user: 'qa-user-r4',
    retarget(t) { current = t; start(); rec.steps.push({ step: 'activeTarget', target: t }); },
    setActive(v) { active = v; rec.steps.push({ step: 'accountActive', value: v }); },
    async index(projectId, file, text) {
      const r = await rag.indexProjectFile(projectId, file, text, ctx.user);
      rec.steps.push({ step: 'index', projectId, file, chars: text.length, version: crypto.createHash('sha256').update(text).digest('hex'), result: r });
      return r;
    },
    note(text) { rec.steps.push({ step: 'note', text }); },
    lock(projectId) {
      const db = new Database(path.join(dataDir, 'users', ctx.user, 'rag', `${projectId}.db`));
      db.exec('BEGIN IMMEDIATE');
      rec.steps.push({ step: 'writeLock', projectId });
      return { close: () => { db.exec('ROLLBACK'); db.close(); } };
    },
    // Deletes the vectors of the first chunks of `first` and `second` and writes them back, `second`'s first: vec0
    // reuses the freed positions in order, so its scan now meets `second`'s chunk before `first`'s although the
    // chunk ids are the other way round (what vector writes by a migration pass can do). Equal scores then come back
    // in scan order, not id order.
    swapFirstVectors(projectId, first, second, table) {
      const db = new Database(path.join(dataDir, 'users', ctx.user, 'rag', `${projectId}.db`));
      sqliteVec.load(db);
      const idOf = (f) => db.prepare('SELECT MIN(id) AS id FROM chunks WHERE file = ?').get(f).id;
      const ids = [idOf(first), idOf(second)];
      const blobs = ids.map((id) => db.prepare(`SELECT embedding FROM ${table} WHERE chunk_id = ?`).get(id).embedding);
      for (const id of ids) db.prepare(`DELETE FROM ${table} WHERE chunk_id = ?`).run(id);
      for (const i of [1, 0]) db.prepare(`INSERT INTO ${table} (chunk_id, embedding) VALUES (${Number(ids[i])}, ?)`).run(blobs[i]);
      db.close();
      rec.steps.push({ step: 'swapFirstVectors', projectId, first, second, chunks: ids });
    },
    remove(projectId, file) {
      rag.deleteProjectFile(projectId, file, ctx.user);
      rec.steps.push({ step: 'delete', projectId, file });
    },
    sql(projectId, statement) {
      const db = new Database(path.join(dataDir, 'users', ctx.user, 'rag', `${projectId}.db`));
      sqliteVec.load(db);
      db.exec(statement);
      db.close();
      rec.steps.push({ step: 'sql', projectId, statement });
    },
    async search(projectId, query, versions, note) {
      // The index as this search reads it (before Node's own open, which may adopt a legacy table).
      const index = ctx.snapshot(projectId);
      lastQuery = null;
      const hits = await rag.searchProject(projectId, query, ctx.user);
      const file = path.join(dataDir, 'users', ctx.user, 'rag', `${projectId}.db`);
      rec.searches.push({ projectId, query, note, target: current, index, created: fs.existsSync(file), query_framed: lastQuery?.framed ?? null, query_vector: lastQuery?.vector ?? null, versions, hits });
      return hits;
    },
    snapshot(projectId) {
      const file = path.join(dataDir, 'users', ctx.user, 'rag', `${projectId}.db`);
      if (!fs.existsSync(file)) return null;
      const dump = dumpIndex(file);
      const key = sha(JSON.stringify(dump)).slice(0, 16);
      snapshots[key] = dump;
      return key;
    },
  };
  return build(ctx).then(() => { scenarios.push(rec); });
}

const sha = (t) => crypto.createHash('sha256').update(t).digest('hex');

(async () => {
  // S1: a vec_<id> slot with its hash map (embeddinggemma-2 framing). F2 small file, F3 large files and vectors,
  // F4 order/score/TOP_K/MIN_SCORE, ties across two files with the same text, a vector whose map hash is stale.
  const longA = prose(60, 1);
  const longB = prose(70, 4, 'hotel', 3);
  const dup = prose(40, 2, 'echo', 2);
  const neg = prose(30, 5).replace(/\./g, ' zulu zulu zulu.');
  await scenario('S1-map-slot', { model: 'qa-embed-map', profile: 'embeddinggemma-2', baseUrl: 'http://127.0.0.1:9' }, async (c) => {
    dim = 8;
    await c.index('qa-proj-1', 'qa-small.md', 'alpha bravo charlie. A small synthetic note.');
    await c.index('qa-proj-1', 'qa-long-a.md', longA);
    await c.index('qa-proj-1', 'qa-long-b.md', longB);
    await c.index('qa-proj-1', 'qa-dup-1.md', dup);
    await c.index('qa-proj-1', 'qa-dup-2.md', dup);
    await c.index('qa-proj-1', 'qa-negative.md', neg);
    // A vector whose map row names other text (an older release edited the chunk): never joined.
    c.sql('qa-proj-1', "UPDATE vecmap_" + slots.slotId({ model: 'qa-embed-map', profile: 'embeddinggemma-2' }) + " SET content_hash = 'qa-stale-hash' WHERE chunk_id = (SELECT MIN(id) FROM chunks WHERE file = 'qa-long-b.md')");
    await c.search('qa-proj-1', 'alpha bravo', {}, 'mixed files');
    await c.search('qa-proj-1', 'echo echo echo', {}, 'ties between the two duplicate files');
    await c.search('qa-proj-1', 'hotel hotel', {}, 'the stale-map chunk of qa-long-b.md is not joined');
    await c.search('qa-proj-1', 'zulu zulu zulu zulu', {}, 'a negative query vector: only the negative file reaches MIN_SCORE');
    await c.search('qa-proj-1', 'golf', {}, 'single word');
    await c.search('qa-proj-1', 'nothing here', {}, 'no vocabulary word: only the per-text offset dimension');
  });

  // S2: the version filter (F5) and delete (F6). Node keeps a wanted version per file while a re-index or delete
  // could not finish; chunks of any other version are dropped AFTER the SQL LIMIT (pool x 4), so a stale file with
  // many strong chunks can leave fewer than TOP_K hits.
  const stale = Array.from({ length: 9 }, (_, k) => prose(60, k * 13 + 3, 'hotel', 2) + ' ' + VOCAB[k % 8] + ' qa marker ' + k + '.').join('\n\n');
  const fresh = prose(50, 6, 'hotel', 4);
  await scenario('S2-versions', { model: 'qa-embed-ver', profile: 'embeddinggemma-2', baseUrl: 'http://127.0.0.1:9' }, async (c) => {
    dim = 8;
    await c.index('qa-proj-2', 'qa-stale.md', stale);
    await c.index('qa-proj-2', 'qa-fresh.md', fresh);
    await c.index('qa-proj-2', 'qa-gone.md', longA);
    await c.search('qa-proj-2', 'hotel', {}, 'before: every file current');
    // A re-index whose write fails (another connection holds the write lock past the 5 s busy timeout): the old
    // chunks stay and the wanted version stays at the new text's hash (rag.cjs pruneFileState keeps it on a failure).
    const changed = stale + ' alpha.';
    const holder = c.lock('qa-proj-2');
    await c.index('qa-proj-2', 'qa-stale.md', changed).catch((e) => c.note(`re-index failed: ${e.code || e.message}`));
    holder.close();
    c.note('write lock released');
    // A delete that cannot open the index (the account is briefly inactive): the null version stays, so every
    // remaining chunk of the file is filtered.
    c.setActive(false);
    c.remove('qa-proj-2', 'qa-gone.md');
    c.setActive(true);
    const versions = { 'qa-stale.md': sha(changed), 'qa-gone.md': null };
    await c.search('qa-proj-2', 'hotel', versions, 'stale file filtered after the LIMIT');
    await c.search('qa-proj-2', 'alpha bravo', versions, 'deleted file filtered');
    await c.search('qa-proj-2', 'charlie delta', versions, 'other words');
  });

  await scenario('S3-delete', { model: 'qa-embed-del', profile: 'embeddinggemma-2', baseUrl: 'http://127.0.0.1:9' }, async (c) => {
    dim = 8;
    await c.index('qa-proj-3', 'qa-keep.md', longB);
    await c.index('qa-proj-3', 'qa-drop.md', longA);
    await c.search('qa-proj-3', 'alpha', {}, 'before delete');
    c.remove('qa-proj-3', 'qa-drop.md');
    await c.search('qa-proj-3', 'alpha', {}, 'after delete: no row, no vector, nothing pending');
  });

  // S4: the legacy table vec_items (nomic-v1, raw text, no map). A fresh nomic-v1 index uses it as its slot.
  await scenario('S4-legacy-slot', { model: 'qa-embed-legacy', profile: 'nomic-v1', baseUrl: 'http://127.0.0.1:9' }, async (c) => {
    dim = 6;
    await c.index('qa-proj-4', 'qa-legacy-a.md', longA);
    await c.index('qa-proj-4', 'qa-legacy-b.md', dup);
    await c.search('qa-proj-4', 'alpha bravo', {}, 'legacy slot');
    wrongWidthFor = 'qa-wide-query';
    await c.search('qa-proj-4', 'qa-wide-query alpha', {}, 'query vector wider than the slot: no hits');
    wrongWidthFor = null;
  });

  // S5: a pre-#1636 index: vec_items and vec_meta 'dim', no slot record. Node adopts it for a nomic-v1 target on
  // open (writes the slot record) and searches it.
  await scenario('S5-unclaimed-legacy', { model: 'qa-embed-legacy', profile: 'nomic-v1', baseUrl: 'http://127.0.0.1:9' }, async (c) => {
    dim = 6;
    await c.index('qa-proj-5', 'qa-old.md', longB);
    c.sql('qa-proj-5', "DELETE FROM vec_meta WHERE key LIKE 'slot:%'");
    await c.search('qa-proj-5', 'hotel bravo', {}, 'adopted on open');
  });

  // S6: the index has only another model's slot: no vector search, never another model's vectors.
  await scenario('S6-other-model', { model: 'qa-embed-other', profile: 'embeddinggemma-2', baseUrl: 'http://127.0.0.1:9' }, async (c) => {
    dim = 8;
    await c.index('qa-proj-6', 'qa-a.md', longA);
    c.retarget({ model: 'qa-embed-new', profile: 'embeddinggemma-2', baseUrl: 'http://127.0.0.1:9' });
    await c.search('qa-proj-6', 'alpha', {}, 'no slot for the active model');
    await c.search('qa-proj-missing', 'alpha', {}, 'no index file: [] and nothing created');
  });
  // S7: equal scores in Node come back in the vec0 scan order. The native search orders them by chunk id instead
  // (a defined order); this records the one shape where the two differ, as a named difference.
  await scenario('S7-tie-order', { model: 'qa-embed-ties', profile: 'embeddinggemma-2', baseUrl: 'http://127.0.0.1:9' }, async (c) => {
    dim = 8;
    await c.index('qa-proj-7', 'qa-twin-1.md', dup);
    await c.index('qa-proj-7', 'qa-twin-2.md', dup);
    await c.search('qa-proj-7', 'echo echo echo', {}, 'ties in id order (scan order = id order)');
    c.swapFirstVectors('qa-proj-7', 'qa-twin-1.md', 'qa-twin-2.md', 'vec_' + slots.slotId({ model: 'qa-embed-ties', profile: 'embeddinggemma-2' }));
    await c.search('qa-proj-7', 'echo echo echo', {}, 'difference: tie-order (Node: scan order; native: chunk id order)');
  });

  const out = path.join(__dirname, 'fixtures', 'F2-F6-search.json');
  fs.writeFileSync(out, JSON.stringify({
    sources: sourcesSha,
    source: 'server/rag.cjs searchProject/searchProjectNow/textHitsConfigured :878-922, isCurrentVersion :866-871, indexProjectFile :630-700, deleteProjectFile :835-862; server/vector-slots.cjs joinSql :147, adoptLegacy :59, findSlot :69',
    constants: { TOP_K: 6, MIN_SCORE: 0.3, LIMIT: 24 },
    vocab: VOCAB,
    scenarios,
    snapshots,
  }, null, 1) + '\n');
  fs.rmSync(tmpRoot, { recursive: true, force: true });
  process.stdout.write(`wrote ${out}: ${scenarios.length} scenarios, ${scenarios.reduce((n, s) => n + s.searches.length, 0)} searches, ${calls.length} embedding calls\n`);
})().catch((e) => { process.stderr.write(String(e.stack || e) + '\n'); fs.rmSync(tmpRoot, { recursive: true, force: true }); process.exit(1); });
