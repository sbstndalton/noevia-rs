#!/usr/bin/env node
'use strict';
// Index-write fixtures for crates/project-index, recorded from noevia-core's own server/rag.cjs and
// server/vector-slots.cjs with better-sqlite3 + sqlite-vec, on synthetic QA-named projects. No model and no network:
// the embeddings endpoint is the test seam (_test.setRagFetch) answering with deterministic word-count vectors of the
// unframed chunk text.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout with server/node_modules installed> \
//     node crates/project-index/tests/generate_index_fixtures.cjs
//
// Writes tests/fixtures/chunk-text.json (chunkText over edge-case texts) and tests/fixtures/index-steps.json: per
// scenario the steps (index, tail append, delete, busy, embedding failure, retarget), each with Node's result, its
// in-process state sizes (trackedFileState) and the index as Node left it. The Rust test replays the steps.
process.env.NODE_ENV = 'test';
const crypto = require('crypto');
const fs = require('fs');
const os = require('os');
const path = require('path');
const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) { process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n'); process.exit(2); }
const srv = path.join(path.resolve(checkout), 'server');
for (const k of Object.keys(process.env)) if (/^(RERANK_|RAG_|NOEVIA_FEATURE_RAG|EMBEDDING_|FILES_CONTEXT)/.test(k)) delete process.env[k];
console.warn = () => {};
const sha = (rel) => crypto.createHash('sha256').update(fs.readFileSync(path.join(srv, rel))).digest('hex');
const sources = { 'server/rag.cjs': sha('rag.cjs'), 'server/vector-slots.cjs': sha('vector-slots.cjs'), 'server/embedding-profile.cjs': sha('embedding-profile.cjs') };

const rag = require(path.join(srv, 'rag.cjs'));
const profiles = require(path.join(srv, 'embedding-profile.cjs')).profileNames();
const Database = require(path.join(srv, 'node_modules', 'better-sqlite3'));
const sqliteVec = require(path.join(srv, 'node_modules', 'sqlite-vec'));
const LEGACY = 'nomic-v1';
const MODERN = profiles.find((p) => p !== LEGACY);

// ---- chunkText ----
const words = ['alpha', 'bravo', 'charlie', 'delta', 'echo', 'foxtrot', 'golf', 'hotel'];
function prose(sentences, seed, sep = '.') {
  const out = [];
  for (let s = 0; s < sentences; s++) {
    const n = 8 + ((s * 5 + seed) % 7);
    const ws = [];
    for (let i = 0; i < n; i++) ws.push(words[(i * 3 + s * 7 + seed) % words.length]);
    out.push(ws.join(' ') + sep);
    if (s % 6 === 5) out.push('\n\n');
  }
  return out.join(' ').replace(/ \n\n /g, '\n\n');
}
const texts = {
  empty: '', blank: ' \r\n\t　 ', short: '  one line\r\nsecond  ', exact: 'x'.repeat(1200), over: 'x'.repeat(1201),
  long_flat: 'y'.repeat(5000), prose: prose(80, 1), prose_bang: prose(60, 3, '!'), crlf: prose(50, 2).replace(/\n/g, '\r\n'),
  // A surrogate pair across the 1200 boundary: Node cuts between its halves.
  astral: 'a'.repeat(1199) + '\u{1F600}' + 'b'.repeat(1500),
  unicode_space: '  ' + prose(40, 4) + '﻿ ',
  pages: '[Page 1]\n' + prose(30, 5) + '\n\n[Page 2]\n' + prose(4, 6) + '\n\n[Page 3]\n \n\n[Page 4]\n' + prose(45, 7),
  page_no_newline: '[Page 1]' + prose(30, 8),
  inline_marker: 'intro\n[Page 2]\n' + prose(30, 9),
  dupes: ('same paragraph. '.repeat(100) + '\n\n').repeat(4),
};
// A chunk that ends inside a surrogate pair is written as UTF-16 code units (JSON cannot carry it as text).
const codeUnits = (s) => Array.from({ length: s.length }, (_, i) => s.charCodeAt(i));
const wellFormed = (c) => !/[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/.test(c);
const chunkFixture = { sources, cases: Object.entries(texts).map(([name, text]) => ({ name, text, chunks: rag.chunkText(text).map((c) => (wellFormed(c) ? c : codeUnits(c))) })) };

// ---- index steps ----
function bodyOf(t) { return String(t).replace(/^title: [^|]*\| text: /, ''); }
function vectorFor(text, dim) {
  const ws = String(text).toLowerCase().match(/[a-z]+/g) || [];
  const v = new Array(dim).fill(0);
  for (const w of ws) { const i = words.indexOf(w); if (i >= 0 && i < dim - 1) v[i] += 1; }
  const h = crypto.createHash('sha256').update(String(text)).digest();
  v[dim - 1] += h[0] / 2550;
  return v.map((x) => Math.fround(x));
}
let dim = 6;
let failing = false;
let shortBatches = false;
const calls = [];
rag._test.setRagFetch(async (url, init) => {
  const body = JSON.parse(init.body);
  calls.push(body.input.length);
  if (failing) return new Response('down', { status: 500 });
  if (shortBatches && body.input.length > 1) return new Response(JSON.stringify({ data: [] }), { status: 200 });
  const data = body.input.map((t, index) => ({ index, embedding: vectorFor(bodyOf(t), dim) })).reverse();
  return new Response(JSON.stringify({ data }), { status: 200, headers: { 'Content-Type': 'application/json' } });
});

const tmpRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-p1-index-'));
function dumpIndex(file) {
  if (!fs.existsSync(file)) return null;
  const db = new Database(file, { readonly: true });
  sqliteVec.load(db);
  const master = db.prepare("SELECT type, name, sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY rowid").all();
  const virtualNames = master.filter((m) => /CREATE VIRTUAL TABLE/i.test(m.sql)).map((m) => m.name);
  const chunks = db.prepare('SELECT id, file, chunk_no, body, content_hash, version, direct, kind, meta FROM chunks ORDER BY id').all();
  const vecMeta = db.prepare('SELECT key, value FROM vec_meta ORDER BY key').all();
  const vectors = {};
  for (const v of virtualNames) {
    vectors[v] = db.prepare(`SELECT chunk_id, embedding FROM ${v} ORDER BY chunk_id`).all()
      .map((r) => ({ chunk_id: r.chunk_id, embedding: Array.from(new Float32Array(r.embedding.buffer, r.embedding.byteOffset, r.embedding.length / 4)) }));
  }
  const maps = {};
  for (const m of master.filter((x) => x.type === 'table' && /^vecmap_[0-9a-f]{16}$/.test(x.name))) {
    maps[m.name] = db.prepare(`SELECT chunk_id, content_hash FROM ${m.name} ORDER BY chunk_id`).all();
  }
  const columns = db.prepare('PRAGMA table_info(chunks)').all().map((c) => c.name);
  db.close();
  return { columns, chunks, vec_meta: vecMeta, vectors, maps };
}
const tick = () => new Promise((r) => setImmediate(r));

async function scenario(name, target, build) {
  const dataDir = path.join(tmpRoot, name);
  fs.mkdirSync(dataDir, { recursive: true });
  const user = 'qa-user-p1';
  let current = target;
  const start = () => rag.init({ dataDir, inferenceUrl: 'http://127.0.0.1:9/v1', userDataDirFn: (u) => path.join(dataDir, 'users', u),
    userActive: () => true, embeddingConfig: { active: () => ({ ...current }) } });
  start();
  const base = rag.trackedFileState();
  const file = (p) => path.join(dataDir, 'users', user, 'rag', `${p}.db`);
  const rec = { name, target, steps: [] };
  const push = async (step, projectId, result) => {
    await tick(); await tick();
    const t = rag.trackedFileState();
    rec.steps.push({ ...step, projectId, result: result === undefined ? null : result, calls: calls.splice(0),
      tracked: { generations: t.generations - base.generations, versions: t.versions - base.versions }, index: dumpIndex(file(projectId)) });
  };
  let leave = null;
  const ctx = {
    async index(p, f, text) { await push({ step: 'index', file: f, text }, p, await rag.indexProjectFile(p, f, text, user)); },
    async tail(p, f, headText, opts = {}) {
      const tail = rag.chunkText(headText + (opts.extra || '')).map((body) => ({ body, page: null }));
      const o = { limit: opts.limit ?? 8, fromPage: opts.fromPage ?? null, toPage: opts.toPage ?? null, final: opts.final ?? true };
      await push({ step: 'tail', file: f, headText, tail, opts: o }, p, await rag.appendFileTail(p, f, headText, tail, user, o));
    },
    async remove(p, f, keepMedia = false) { rag.deleteProjectFile(p, f, user, { keepMedia }); await push({ step: 'delete', file: f, keepMedia }, p); },
    async busy(on) { if (on) leave = rag.foreground(); else { leave(); leave = null; } rec.steps.push({ step: 'busy', on }); },
    async fail(on) { failing = on; rec.steps.push({ step: 'fail', on }); },
    async short(on) { shortBatches = on; rec.steps.push({ step: 'short', on }); },
    async retarget(t) { current = t; start(); rec.steps.push({ step: 'retarget', target: t }); },
  };
  await build(ctx);
  return rec;
}

const legacy = { profile: LEGACY, model: 'qa-model-a' };
const modern = { profile: MODERN, model: 'qa-model-b' };
const long = (seed, n = 40) => prose(n, seed);
const paged = (from, to, seed) => Array.from({ length: to - from + 1 }, (_, i) => `[Page ${from + i}]\n${prose(14, seed + i)}`).join('\n\n');

(async () => {
  const scenarios = [];
  for (const target of [legacy, modern]) {
    const tag = target === legacy ? 'legacy' : 'modern';
    scenarios.push(await scenario(`small-and-chunked-${tag}`, target, async (c) => {
      await c.index('qa-proj', 'small.md', 'alpha bravo small note');
      await c.index('qa-proj', 'long.md', long(1));
      await c.index('qa-proj', 'long.md', long(1)); // unchanged: same rows, re-embedded
      await c.index('qa-proj', 'long.md', long(2)); // changed: replaced
      await c.index('qa-proj', 'dupes.md', texts.dupes);
      await c.index('qa-proj', 'small.md', long(3)); // a small file grows
      await c.index('qa-proj', 'long.md', 'now small');
      await c.remove('qa-proj', 'small.md');
      await c.remove('qa-proj', 'missing.md');
    }));
  }
  scenarios.push(await scenario('embed-failures', modern, async (c) => {
    await c.fail(true);
    await c.index('qa-proj', 'a.md', long(4));
    await c.fail(false);
    await c.short(true);
    await c.index('qa-proj', 'b.md', long(5));
    await c.short(false);
  }));
  scenarios.push(await scenario('tail', modern, async (c) => {
    const head = paged(1, 12, 10);
    const extra = '\n\n' + paged(13, 30, 40);
    await c.tail('qa-proj', 'doc.pdf', head, { extra }); // no index yet
    await c.index('qa-proj', 'doc.pdf', head);
    await c.tail('qa-proj', 'other.pdf', head, { extra }); // head not indexed
    await c.tail('qa-proj', 'doc.pdf', 'something else entirely', { extra }); // another text: reindex-pending or head-not-indexed
    await c.tail('qa-proj', 'doc.pdf', head, { extra, limit: 5 });
    await c.tail('qa-proj', 'doc.pdf', head, { extra, limit: 5, fromPage: 13, toPage: 20, final: false });
    await c.busy(true);
    await c.tail('qa-proj', 'doc.pdf', head, { extra, limit: 5 });
    await c.busy(false);
    await c.fail(true);
    await c.tail('qa-proj', 'doc.pdf', head, { extra, limit: 3 });
    await c.fail(false);
    await c.tail('qa-proj', 'doc.pdf', head, { extra, limit: 100 });
    await c.tail('qa-proj', 'doc.pdf', head, { extra: '\n\n' + paged(13, 22, 40), limit: 100 }); // record shrank
    await c.index('qa-proj', 'doc.pdf', head); // re-index of the same text keeps the tail
    await c.index('qa-proj', 'doc.pdf', paged(1, 12, 11)); // a new head drops the tail
    await c.remove('qa-proj', 'doc.pdf', true);
  }));
  scenarios.push(await scenario('retarget', legacy, async (c) => {
    await c.index('qa-proj', 'a.md', long(6));
    await c.retarget(modern);
    await c.index('qa-proj', 'b.md', long(7));
    await c.remove('qa-proj', 'a.md');
  }));
  const dir = path.join(__dirname, 'fixtures');
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'chunk-text.json'), JSON.stringify(chunkFixture, null, 1) + '\n');
  fs.writeFileSync(path.join(dir, 'index-steps.json'), JSON.stringify({ sources, dim, scenarios }, null, 1) + '\n');
  fs.rmSync(tmpRoot, { recursive: true, force: true });
})().catch((e) => { process.stderr.write(String(e.stack || e) + '\n'); process.exit(1); });
