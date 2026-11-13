#!/usr/bin/env node
'use strict';
// F14 of handoff/foundation/R4.md: searchProjectWithImages (#1726), recorded from noevia-core's own server/rag.cjs
// with better-sqlite3 + sqlite-vec 0.1.9 on a synthetic QA project. Text chunks go in through indexProjectFile,
// image rows through syncProjectMedia (its `embedParts` seam answers the image vectors), both with deterministic
// word-count vectors; then every search's { text, images, indexed } and the index it read.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout with server/node_modules installed> \
//     node crates/project-retrieval/tests/generate_image_fixtures.cjs
//
// RAG_IMAGE_TOP_K and RAG_IMAGE_MIN_SCORE are read once when rag.cjs loads, so each setting runs in a child process.
// Writes crates/project-retrieval/tests/fixtures/F14-image-search.json.
process.env.NODE_ENV = 'test';
const crypto = require('crypto');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFileSync } = require('child_process');
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
const SETTINGS = [
  { name: 'defaults', env: {} },
  { name: 'top 4, floor 0.5', env: { RAG_IMAGE_TOP_K: '4', RAG_IMAGE_MIN_SCORE: '0.5' } },
  { name: 'top 0', env: { RAG_IMAGE_TOP_K: '0' } },
  { name: 'top 99 clamps to 8, floor -1', env: { RAG_IMAGE_TOP_K: '99', RAG_IMAGE_MIN_SCORE: '-1' } },
  { name: 'unusable values fall back', env: { RAG_IMAGE_TOP_K: 'many', RAG_IMAGE_MIN_SCORE: '1.5' } },
  { name: 'top 2.9 parses as 2, floor as a number', env: { RAG_IMAGE_TOP_K: '2.9', RAG_IMAGE_MIN_SCORE: ' 0.25 ' } },
];

if (process.argv[2] === 'child') {
  record().then((o) => process.stdout.write(JSON.stringify(o)), (e) => { process.stderr.write(String(e.stack || e)); process.exit(1); });
} else {
  const settings = SETTINGS.map((s) => {
    const env = { ...process.env, ...s.env };
    for (const k of Object.keys(env)) if (/^(RERANK_|NOEVIA_FEATURE_RAG|EMBEDDING_|FILES_CONTEXT)/.test(k) || (/^RAG_/.test(k) && !(k in s.env))) delete env[k];
    env.EMBEDDING_IMAGE_MODEL = 'qa-vision';
    const out = JSON.parse(execFileSync(process.execPath, [__filename, 'child'], { env, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 }));
    return { name: s.name, env: s.env, ...out };
  });
  const snapshots = {};
  for (const s of settings) { Object.assign(snapshots, s.snapshots); delete s.snapshots; }
  const out = path.join(__dirname, 'fixtures', 'F14-image-search.json');
  fs.writeFileSync(out, JSON.stringify({
    sources: SOURCES,
    source: 'server/rag.cjs searchProjectWithImages/searchProjectWithImagesNow :934-970, mediaRow :973-979, IMAGE_TOP_K/IMAGE_MIN_SCORE :971-972, syncProjectMedia :1045-1123',
    settings,
    snapshots,
  }, null, 1) + '\n');
  process.stdout.write(`wrote ${out}: ${settings.length} settings, ${settings.reduce((n, s) => n + s.searches.length, 0)} searches\n`);
}

function record() {
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
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-r4-images-'));
  const USER = 'qa-user-r4';
  const target = { model: 'qa-embed-mm', profile: 'embeddinggemma-2', baseUrl: 'http://127.0.0.1:9' };
  rag.init({ dataDir: dir, inferenceUrl: 'http://127.0.0.1:9/v1', userDataDirFn: (u) => path.join(dir, 'users', u), userActive: () => true, embeddingConfig: { active: () => ({ ...target }) } });
  const file = path.join(dir, 'users', USER, 'rag', 'qa-proj-img.db');
  const snapshots = {};
  const snapshot = () => {
    const db = new Database(file, { readonly: true });
    sqliteVec.load(db);
    const master = db.prepare('SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY rowid').all();
    const virt = master.filter((m) => /CREATE VIRTUAL TABLE/i.test(m.sql)).map((m) => m.name);
    const dump = {
      schema: master.filter((m) => !virt.some((v) => m.name !== v && m.name.startsWith(`${v}_`)) && !m.name.startsWith('sqlite_')).map((m) => m.sql),
      chunks: db.prepare('SELECT id, file, chunk_no, body, content_hash, version, direct, kind, meta FROM chunks ORDER BY id').all(),
      vec_meta: db.prepare('SELECT key, value FROM vec_meta ORDER BY rowid').all(),
      vectors: Object.fromEntries(virt.map((v) => [v, db.prepare(`SELECT chunk_id, embedding FROM ${v}`).all().map((r) => ({ chunk_id: r.chunk_id, embedding: Array.from(new Float32Array(r.embedding.buffer, r.embedding.byteOffset, r.embedding.length / 4)) }))])),
      maps: Object.fromEntries(master.filter((x) => x.type === 'table' && /^vecmap_[0-9a-f]{16}$/.test(x.name)).map((m) => [m.name, db.prepare(`SELECT chunk_id, content_hash FROM ${m.name} ORDER BY rowid`).all()])),
    };
    db.close();
    const key = crypto.createHash('sha256').update(JSON.stringify(dump)).digest('hex').slice(0, 16);
    snapshots[key] = dump;
    return key;
  };
  const words = (n, seed, bias) => Array.from({ length: n }, (_, i) => (i % 2 === 0 && bias ? bias : VOCAB[(i * 3 + seed) % VOCAB.length])).join(' ');
  const prose = (sentences, seed, bias) => Array.from({ length: sentences }, (_, s) => words(9 + (s % 5), s + seed, bias) + '.').join(' ');
  const img = (name, label, meta) => ({ file: name, hash: `img:${crypto.createHash('sha256').update(label).digest('hex')}`, label, meta, part: () => ({ type: 'image_url', image_url: { url: `data:image/png;base64,${Buffer.from(label).toString('base64')}` } }) });
  const searches = [];
  return (async () => {
    await rag.indexProjectFile('qa-proj-img', 'qa-notes.md', prose(40, 1, 'alpha'), USER);
    await rag.indexProjectFile('qa-proj-img', 'qa-guide.md', prose(40, 4, 'hotel'), USER);
    const items = [
      img('qa-photos.pdf', 'qa figure alpha alpha bravo', { source: 'qa-photos.pdf', page: 1, figure: 1 }),
      img('qa-photos.pdf', 'qa figure hotel hotel golf', { source: 'qa-photos.pdf', page: 2, figure: 1 }),
      img('qa-diagram.png', 'qa diagram alpha charlie delta', { source: 'qa-diagram.png' }),
      img('qa-chart.png', 'qa chart echo foxtrot', { source: 'qa-chart.png', caption: 'QA chart' }),
      img('qa-twin-a.png', 'qa twin alpha echo', { source: 'qa-twin-a.png' }),
      img('qa-twin-b.png', 'qa twin alpha echo', { source: 'qa-twin-b.png' }),
      img('qa-broken.png', 'qa broken alpha bravo', { source: 'qa-broken.png' }),
    ];
    // Image vectors from the label text, as the stub embeds text (one space: a text query finds an image by content).
    const embedParts = async (parts) => parts.map((p) => vectorFor(Buffer.from(p.image_url.url.split(',')[1], 'base64').toString()));
    const synced = await rag.syncProjectMedia('qa-proj-img', USER, items, { limit: 10, embedParts });
    // Meta Node cannot read back: the row is skipped (mediaRow returns null).
    const db = new Database(file); sqliteVec.load(db);
    db.prepare("UPDATE chunks SET meta = 'not json' WHERE kind = 'image' AND file = 'qa-broken.png'").run();
    db.close();
    for (const query of ['alpha bravo', 'hotel golf', 'alpha echo', 'zulu', 'charlie delta alpha']) {
      const index = snapshot();
      lastQuery = null;
      const out = await rag.searchProjectWithImages('qa-proj-img', query, USER);
      searches.push({ query, index, target, user: USER, project: 'qa-proj-img', query_vector: lastQuery, out });
    }
    fs.rmSync(dir, { recursive: true, force: true });
    return { synced, searches, snapshots };
  })();
}
