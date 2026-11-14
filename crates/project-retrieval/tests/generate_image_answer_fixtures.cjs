#!/usr/bin/env node
'use strict';
// F16: the answer side of retrieved images (#1726), recorded from noevia-core's own server/multimodal-rag.cjs on
// synthetic QA cases: assetPlan, resolveHit (a fake workspace and fs: the path each image would be read from) and
// answerImages (scripted canSee/caption/bytes, a visible frame function). No index, no model, no network.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> node crates/project-retrieval/tests/generate_image_answer_fixtures.cjs
//
// Writes crates/project-retrieval/tests/fixtures/F16-image-answer.json (replayed by tests/image_answer_parity.rs).
process.env.NODE_ENV = 'test';
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) { process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n'); process.exit(2); }
const SOURCES = { 'server/multimodal-rag.cjs': '32176635fc344ad4031590bf3332b8d89a983a860d3a95aef5949bdb388df377' };
for (const [rel, want] of Object.entries(SOURCES)) {
  const got = crypto.createHash('sha256').update(fs.readFileSync(path.join(checkout, rel))).digest('hex');
  if (got !== want) { process.stderr.write(`source hash mismatch for ${rel}: ${got}\n`); process.exit(1); }
}
const mm = require(path.join(path.resolve(checkout), 'server/multimodal-rag.cjs'));

const H1 = '1'.repeat(64), H2 = '2'.repeat(64), H3 = '3'.repeat(64), V1 = 'a'.repeat(64), V2 = 'b'.repeat(64);
// A Set as a list; `undefined` (a JS Set holds it, JSON cannot) as { undefined: true }.
const listOf = (set) => [...set].map((v) => (v === undefined ? { undefined: true } : v));

const project = {
  id: 'qa-proj',
  files: [
    { name: 'apple.png', attachment: { id: H1 }, content: '  APPLE label text \n' },
    { name: 'banana.jpg', attachment: { id: H2 } },
    { name: 'docs/report.pdf', document: { availableVersion: V1 } },
    { name: 'pics/kiwi.webp', attachment: { id: H3 } },
    { name: 'bare.png' },
    null,
  ],
  assets: [
    { id: 'img-1', name: 'apple.png', mime: 'image/png', sourceName: 'apple.png' },
    { id: 'img-2', name: 'banana.jpg', mime: 'image/jpeg', sourceName: 'banana.jpg' },
    { id: 'img-3', mime: 'image/webp' },
    { id: 'img-4', name: '', mime: 'image/png' },
    null,
  ],
};
const assetHit = (id, file, hash, score) => ({ file, kind: 'image', hash: `img:${hash}`, score, meta: { source: file, assetId: id } });
const figHit = (extra = {}) => ({ file: 'docs/report.pdf', kind: 'image', hash: `img:${H3}`, score: 0.6,
  meta: { source: 'docs/report.pdf', page: 2, figure: 1, version: V1, figureFile: `${V1}.fig0.png`, mime: 'image/png', caption: ' A chart ', text: 'Q3 revenue\n', ...extra } });

const planCases = [
  { name: 'seen and retrieved', assets: project.assets, hits: [assetHit('img-1', 'apple.png', H1, 0.71)], indexed: [assetHit('img-1', 'apple.png', H1, 0), figHit(), { meta: { assetId: '' } }, null] },
  { name: 'nothing indexed: all ride along', assets: project.assets, hits: [], indexed: [] },
  { name: 'id-less and duplicate assets', assets: [null, { name: 'x' }, { id: 'img-9' }, { id: 'img-9' }, { id: 0 }, { id: 7 }], hits: [{ meta: { assetId: 7 } }], indexed: [{ meta: { assetId: 7 } }] },
];

const resolveCases = [
  { name: 'image source', hit: assetHit('img-1', 'apple.png', H1, 0.71) },
  { name: 'image source changed since indexing', hit: assetHit('img-1', 'apple.png', H2, 0.7) },
  { name: 'source gone', hit: assetHit('img-1', 'gone.png', H1, 0.7) },
  { name: 'asset gone', hit: assetHit('img-7', 'apple.png', H1, 0.7) },
  { name: 'asset without a name', hit: assetHit('img-3', 'pics/kiwi.webp', H3, 0.5) },
  { name: 'asset with an empty name', hit: assetHit('img-4', 'apple.png', H1, 0.5) },
  { name: 'source without attachment id', hit: { file: 'bare.png', hash: 'img:undefined', meta: { assetId: 'img-1' } } },
  { name: 'document figure', hit: figHit() },
  { name: 'figure of an older version', hit: figHit({ version: V2 }) },
  { name: 'figure without documentDir', hit: figHit(), noDocuments: true },
  { name: 'figure file outside the pattern', hit: figHit({ figureFile: `../${V1}.fig0.png` }) },
  { name: 'figure without text, odd numbers', hit: figHit({ page: 3, figure: 2.5, caption: '   ', text: 7 }) },
  { name: 'figure with only text', hit: figHit({ caption: undefined, text: ' only text ' }) },
  { name: 'neither asset nor figure', hit: { file: 'apple.png' } },
  { name: 'hit without a file', hit: { meta: { assetId: 'img-1' }, hash: 'img:undefined' } },
];

const workspace = { assetDir: () => '/a' };
function resolved(c) {
  let readPath = null;
  const fsImpl = { readFileSync: (p) => { readPath = p; return Buffer.from('x'); } };
  const r = mm.resolveHit(workspace, project, c.hit, { documentDir: c.noDocuments ? undefined : () => '/d', fsImpl });
  if (!r) return null;
  r.bytes();
  const { bytes, ...rest } = r;
  return { ...rest, path: readPath };
}

const big = (n) => n;
const answerCases = [
  { name: 'no images', images: [], external: false, sees: true },
  { name: 'off box: names only', images: [{ name: 'a.png', mime: 'image/png', text: 'secret', bytes: 1 }, { name: 'b.png', mime: 'image/png', text: '', bytes: 1 }], external: true, providerLabel: 'Cloud Co', sees: true },
  { name: 'off box, quoting', images: [{ name: 'b "q".png', mime: 'image/png', text: '', bytes: 1 }], external: true, sees: true, knownDifference: 'quoteName' },
  { name: 'sees: caps at three', images: ['1', '2', '3', '4'].map((n) => ({ name: `${n}.png`, mime: 'image/png', text: '', bytes: 3 })), external: false, sees: true, hasVisionRole: true },
  { name: 'sees: byte budget, mime, unreadable', images: [
    { name: 'big.png', mime: 'image/png', text: '', bytes: 10 }, { name: 'next.png', mime: 'image/png', text: 'next text', bytes: 10 },
    { name: 'small.png', mime: 'image/png', text: '', bytes: 5 }, { name: 'odd.bmp', mime: 'image/bmp', text: 'odd text', bytes: 1 },
    { name: 'gone.png', mime: 'image/png', text: 'gone text', bytes: null }], external: false, sees: true, maxTotalBytes: 15 },
  { name: 'sees: over the size limit', images: [{ name: 'huge.png', mime: 'image/png', text: 't', bytes: big(8 * 1024 * 1024 + 1) }], external: false, sees: true, maxTotalBytes: 64 * 1024 * 1024 },
  { name: 'blind: text, captions, names', images: [
    { name: 'a.png', mime: 'image/png', text: 'x'.repeat(8010), bytes: 3 }, { name: 'b.png', mime: 'image/png', text: '', bytes: 3 },
    { name: 'c.png', mime: 'image/png', text: '', bytes: 3 }, { name: 'd.png', mime: 'image/png', text: '', bytes: 3 },
    { name: 'e.png', mime: 'image/png', text: '', bytes: 3 }, { name: 'f.png', mime: 'image/png', text: '', bytes: 3 }],
    external: false, sees: false, hasVisionRole: false,
    captions: { 'b.png': { caption: 'a red apple', model: 'vision-role' }, 'c.png': { caption: 'a lemon', model: null }, 'd.png': { caption: '', model: 'vision-role' }, 'f.png': 'throw' } },
  { name: 'blind, one name, vision role', images: [{ name: 'only.png', mime: 'image/png', text: '', bytes: 3 }], external: false, sees: false, hasVisionRole: true },
  { name: 'probe throws: blind', images: [{ name: 'p.png', mime: 'image/png', text: 'p text', bytes: 3 }], external: false, sees: 'throw' },
  { name: 'sees, max images 1', images: [{ name: 'm1.png', mime: 'image/jpeg', text: '', bytes: 2 }, { name: 'm2.png', mime: 'image/gif', text: 'm2 text', bytes: 2 }], external: false, sees: true, maxImages: 1 },
];

async function answered(c) {
  const images = c.images.map((i) => ({ name: i.name, mime: i.mime, hash: `img:${i.name}`, text: i.text,
    bytes: () => { if (i.bytes === null) throw new Error('gone'); return Buffer.alloc(i.bytes, 0x78); } }));
  let probes = 0;
  const asked = [];
  const out = await mm.answerImages({
    images, external: c.external, ...(c.providerLabel ? { providerLabel: c.providerLabel } : {}),
    canSee: async () => { probes += 1; if (c.sees === 'throw') throw new Error('probe'); return c.sees; },
    caption: async (image) => { asked.push(image.name); const m = (c.captions || {})[image.name]; if (m === 'throw') throw new Error('caption'); return m || null; },
    hasVisionRole: !!c.hasVisionRole, frame: (k, l, t) => `<${k}|${l}|${t}>`,
    ...(c.maxImages ? { maxImages: c.maxImages } : {}), ...(c.maxTotalBytes ? { maxTotalBytes: c.maxTotalBytes } : {}),
  });
  // Parts as their sizes and mimes (a data URL of 8 MB is not a fixture).
  const parts = out.parts.map((p) => { const m = /^data:([^;]+);base64,(.*)$/.exec(p.image_url.url); return { type: p.type, mime: m[1], bytes: Buffer.from(m[2], 'base64').length }; });
  return { ...out, parts, probes, asked };
}

(async () => {
  const fixture = {
    source: SOURCES,
    plan: planCases.map((c) => { const p = mm.assetPlan(c.assets, { hits: c.hits, indexed: c.indexed });
      return { name: c.name, assets: c.assets, hits: c.hits, indexed: c.indexed, out: { rideAlong: listOf(p.rideAlong), retrieved: listOf(p.retrieved), indexed: listOf(p.indexed) } }; }),
    project,
    resolve: resolveCases.map((c) => ({ name: c.name, hit: c.hit, noDocuments: !!c.noDocuments, out: resolved(c) })),
    answer: [],
  };
  for (const c of answerCases) fixture.answer.push({ ...c, out: await answered(c) });
  const out = path.join(__dirname, 'fixtures', 'F16-image-answer.json');
  fs.writeFileSync(out, `${JSON.stringify(fixture, null, 1)}\n`);
  process.stdout.write(`wrote ${out}: ${fixture.plan.length} plans, ${fixture.resolve.length} resolves, ${fixture.answer.length} answers\n`);
})().catch((err) => { process.stderr.write(`${err.stack || err}\n`); process.exit(1); });
