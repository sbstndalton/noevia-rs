#!/usr/bin/env node
'use strict';
// Recorded answers of noevia-core server/document-sources.cjs (notes, noteText, problem, problemNotes, notice,
// readPages) for crates/document-read: tests/node_document_read.rs replays every case against the port.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/document-read/tests/record_document_read.cjs > crates/document-read/tests/fixtures/node-document-read.v1.json
//   ... node crates/document-read/tests/record_document_read.cjs --check crates/document-read/tests/fixtures/node-document-read.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every expectation comes from core's own code; the inputs are synthetic and QA-named (hand-made edges plus a
// seeded PRNG over field pools), passed through JSON first so Node reads exactly what is recorded. readPages reads
// stored results this script writes into a temporary data directory (project-documents/<sha256 of the id>/).
// A thrown error is recorded by kind: `status` (+ message) when it carries one, else `invalid` (versionPath's
// message), `unreadable` (an fs or JSON.parse error) or `thrown` (a TypeError). Each source file's sha256 is
// recorded, so a changed Node file needs a fresh recording. Nothing from node_modules is loaded.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => process.stderr.write(`${a.map(String).join(' ')}\n`);
const SOURCE_FILES = ['server/document-sources.cjs', 'server/quoted-name.cjs', 'server/documents.cjs'];
const sources = Object.fromEntries(SOURCE_FILES.map((f) => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex')]));
const ds = require(path.join(root, 'server', 'document-sources.cjs'));

// Seeded PRNG (mulberry32) so the recording is reproducible.
let seed = 0x5113;
const rand = () => { seed |= 0; seed = (seed + 0x6d2b79f5) | 0; let t = Math.imul(seed ^ (seed >>> 15), 1 | seed); t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t; return ((t ^ (t >>> 14)) >>> 0) / 4294967296; };
const pick = (list) => list[Math.floor(rand() * list.length)];
const json = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

const V = 'a'.repeat(64), OTHER = 'b'.repeat(64);
const page = (number, status, extra = {}) => ({ number, status, ...extra });
const many = (n, status) => Array.from({ length: n }, (_, i) => page(i + 1, status, i % 3 ? {} : { reason: `QA reason ${i}` }));
const POOLS = {
  state: ['ready', 'partial', 'failed', 'queued', undefined, 5, {}, ['qa', 'state'], null, ''],
  stale: [true, false, 1, '', undefined],
  pages: [3, 0, '12', undefined, null, [1, 2], {}, 2.5],
  truncated: [true, undefined, 'yes', 0],
  pageStatus: [undefined, null, [], 0, false, [page(1, 'native'), page(2, 'blank'), page(3, 'ocr')],
    [page(1, 'native'), page(2, 'failed', { reason: 'QA timeout' }), page(3, 'degraded')],
    many(25, 'failed'), many(21, 'skipped'),
    [page(1, 'failed', { retry: { outcome: 'failed', error: 'busy' } }), page(2, 'failed', { retry: { outcome: 'failed', error: 'busy' } }),
      page(3, 'failed', { retry: { outcome: 'failed', error: 5 } }), page(4, 'failed', { retry: { outcome: 'failed', error: '5' } }),
      page(5, 'failed', { retry: { outcome: 'failed' } }), page(6, 'ocr', { retry: { outcome: 'recovered' } }),
      page(7, 'failed', { retry: { outcome: 'empty' } }), page(8, 'failed', { retry: { outcome: 'unavailable' } }),
      page(9, 'skipped', { retry: { outcome: 'skipped' } }), page(10, 'failed', { retry: { outcome: 'failed', error: { qa: 1 } } }),
      page(11, 'failed', { retry: { outcome: 'failed', error: { qa: 1 } } }), page(12, 'failed', { retry: 'failed' })],
    [5, 'qa', page(2, 'native'), { status: 'failed' }, { number: [3, 4], status: 'failed', retry: { outcome: 'skipped' } }],
    Array.from({ length: 23 }, (_, i) => page(i + 1, 'failed', { retry: { outcome: i % 2 ? 'skipped' : 'recovered' } })),
    [{ status: 'failed', retry: { outcome: 'skipped' } }, { number: null, status: 'failed', retry: { outcome: 'skipped' } },
      page(3, 'failed', { retry: { outcome: 'failed', error: true } }), page(4, 'failed', { retry: { outcome: 'failed', error: ['x', 'y'] } }),
      page(5, 'failed', { retry: { outcome: 'failed', error: true } }), page(6, 'failed', { retry: { outcome: 'failed', error: ['x', 'y'] } })],
    [page(1, 'native'), null], 'qa-not-a-list', { qa: 1 }, [page(1, { toString: 1 })]],
  stoppedAt: [undefined, { page: 3, limit: 'pages' }, { page: 2, limit: 'error', error: 'QA busy' }, { limit: 'text' },
    { page: 1, limit: 'constructor' }, { page: 1, limit: '__proto__' }, { page: 1, limit: 'toString' }, { page: 1, limit: 'hasOwnProperty' },
    { page: 'x', limit: ['pages'] }, { page: 4, limit: 'other', error: '' }, 'qa-stop', 5, { page: { qa: 1 }, limit: 7 }, { page: 1, limit: { toString: 1 } }],
  searchable: [undefined, { version: V, complete: false, page: 7 }, { version: OTHER, complete: false, page: 7 }, { complete: false, page: 7 },
    { version: V, complete: false, page: 7.5 }, { version: V, complete: 0, page: 3 }, { version: V, complete: false, page: '7' }, 'qa'],
  error: [undefined, 'Some pages failed.', 'QA failure', 'ends with dots..', 5, {}, '', ['a.', 'b.']],
  version: [OTHER, undefined, 0, ['qa-version'], ''],
  availableVersion: [V, undefined, '', OTHER],
  indexing: ['ready', undefined, 'partial', 0, 'failed'],
  name: ['QA-report.pdf', 'Synthetic/"quote" \\ name.pdf', 'QA line\u2028sep\u202Eflip\u0085.pdf', 'QA lone \ud83d half.pdf', 'QA tab\tctl\u0001.pdf', 'QA marks\u200E\u200F\u202A\u202B\u202C\u202D\u2066\u2067\u2068\u2069.pdf', undefined, 42, {}, ['QA', 'list']],
};
const randomDocument = () => {
  const d = {};
  for (const k of ['state', 'stale', 'pages', 'truncated', 'pageStatus', 'stoppedAt', 'searchable', 'error', 'version', 'availableVersion', 'indexing']) {
    const v = pick(POOLS[k]);
    if (v !== undefined) d[k] = v;
  }
  return d;
};
const files = [
  null, {}, { name: 'QA-plain.md', content: 'x' },
  ...[undefined, null, 0, '', 'qa-doc', [], {}, true].map((document) => ({ name: 'QA-doc.pdf', document })),
  { name: 'QA-queued.pdf', document: { state: 'queued' } }, { name: 'QA-queued-stale.pdf', document: { state: 'queued', stale: 1, pageStatus: 'bad' } },
  { name: { toString: 1 }, document: { state: 'ready' } }, { name: { toString: 1 } },
  { name: 'QA-full.pdf', document: { version: OTHER, availableVersion: V, state: 'partial', pages: 3, stale: true, truncated: true,
    pageStatus: [page(1, 'native'), page(2, 'ocr'), page(3, 'failed', { retry: { outcome: 'failed', error: 'busy' } })],
    stoppedAt: { page: 3, limit: 'pages' }, searchable: { version: V, complete: false, page: 2 }, indexing: 'ready', error: 'Some pages failed.' } },
  { name: 'QA-notes-dot.pdf', document: { state: 'failed', stoppedAt: { page: 2, limit: 'error', error: 'QA read stopped.' } } },
];
for (const name of POOLS.name) files.push({ name, document: { state: 'ready', version: OTHER } });
for (let i = 0; i < 260; i++) files.push({ name: pick(POOLS.name), document: randomDocument() });

const attempt = (fn) => {
  try { return { ok: json(fn()) }; } catch (err) {
    if (err && err.status) return { status: err.status, message: err.message };
    if (err && err.message === 'invalid document version') return { error: 'invalid' };
    if (err && (err.code || err instanceof SyntaxError)) return { error: 'unreadable' };
    if (err instanceof TypeError) return { error: 'thrown' };
    throw err;
  }
};
const fileCases = files.map((f) => {
  const file = json(f) ?? null;
  return { file, notice: attempt(() => ds.notice(file)), problem: attempt(() => ds.problem(file)),
    problemBase: attempt(() => ds.problem(file, { notes: false })), notes: attempt(() => ds.notes(file)),
    problemNotes: attempt(() => ds.problemNotes(file)) };
});

const noteInputs = [null, 5, 'qa', {}, { id: 'unknown' }, { id: 'stoppedAtPage' }, { id: 'stoppedAtPage', params: 'qa' },
  ...['pages', 'text', 'error', 'other', 'constructor', '__proto__', '__defineGetter__', '__defineSetter__', 'hasOwnProperty', '__lookupGetter__',
    '__lookupSetter__', 'isPrototypeOf', 'propertyIsEnumerable', 'toString', 'valueOf', 'toLocaleString', 7, null, ['text'], {}, ['a', 'b']]
    .map((limit) => ({ id: 'stoppedAtPage', params: { page: 3, limit, reason: 'QA reason' } })),
  { id: 'stoppedAtPage', params: { limit: { toString: 1 } } }, { id: 'stoppedAtPage', params: { page: 2, limit: 'pages', reason: '' } },
  ...['ocrRetryFailed', 'ocrRetryEmpty', 'ocrRetryUnavailable', 'ocrRetryLimit', 'searchableToPage', 'ocrRetryRecovered']
    .flatMap((id) => [{ id, params: { pages: '1, 2', page: 4, reason: 'busy' } }, { id }, { id, params: { pages: [1, null, 3], page: [], reason: { qa: 1 } } }]),
  { id: 'ocrRetryFailed', params: { pages: '1', reason: { toString: 1 } } }];
const noteCases = noteInputs.map((n) => { const note = json(n) ?? null; return { note, text: attempt(() => ds.noteText(note)) }; });

// Stored results, by version.
const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-document-read-'));
const PROJECT = 'qa-project-r13';
const docs = path.join(dataDir, 'project-documents', crypto.createHash('sha256').update(PROJECT).digest('hex'));
fs.mkdirSync(docs, { recursive: true });
const ver = (n) => crypto.createHash('sha256').update(`qa-version-${n}`).digest('hex');
const pageTexts = (n, fill) => Array.from({ length: n }, (_, i) => ({ number: i + 1, status: i % 4 === 1 ? 'ocr' : 'native', ...(i % 4 === 1 ? { reason: 'QA scan' } : {}), text: fill(i) }));
const stored = {
  normal: JSON.stringify({ pages: 3, pageTexts: [{ number: 1, status: 'native', text: 'QA first page' },
    { number: 2, status: 'ocr', reason: 'scan', text: 'x😀'.repeat(3000) }, { number: 3, status: 'failed', text: '' }] }),
  bom: '\ufeff' + JSON.stringify({ pages: 1, pageTexts: [{ number: 1, status: 'native', text: 'QA' }] }),
  nullJson: 'null', notJson: '{"pages": 3,', number: '5', array: '[]',
  stringy: JSON.stringify({ pages: '3', pageTexts: [{ number: '1', status: 'native', text: 'QA one' }, { number: ' 2 ', status: 'ocr', text: 'QA two' }, { number: '0x3', status: 'native' }] }),
  notList: JSON.stringify({ pages: 3, pageTexts: 'qa' }), nullEntry: JSON.stringify({ pages: 3, pageTexts: [{ number: 1, status: 'native', text: 'a' }, null] }),
  noPages: JSON.stringify({ pageTexts: pageTexts(7, (i) => (i === 3 ? '' : `QA page ${i + 1} `.repeat(120))) }),
  lone: JSON.stringify({ pages: 2, pageTexts: [{ number: 1, status: 'native', text: 'QA\ud83d' }, { number: 2, status: 'native', text: '\ude00QA' }] }),
  arrays: JSON.stringify({ pages: [2], pageTexts: [{ number: [1], status: ['native', 'x'], reason: ['QA'], text: ['a', 'b'] }, { number: [2], status: null, text: 0 }] }),
  badPages: JSON.stringify({ pages: { toString: 1 }, pageTexts: [] }),
  badNumber: JSON.stringify({ pages: 3, pageTexts: [{ number: { toString: 1 }, status: 'native' }] }),
  objPages: JSON.stringify({ pages: { qa: 1 }, pageTexts: [{ number: 1, status: 'native', text: 'QA' }] }),
  dupPages: JSON.stringify({ pages: 3, pageTexts: [{ number: 1, status: 'native', text: 'a' }, { number: 1, status: 'ocr', text: 'b' }] }),
  hexPages: JSON.stringify({ pages: '0x200000000000018', pageTexts: [{ number: '0x200000000000018', status: 'native', text: 'QA hex' }, { number: '0b11', status: 'native', text: 'QA bin' }] }),
  long: JSON.stringify({ pages: 5, pageTexts: pageTexts(5, (i) => `QA long ${i} ${'é😀'.repeat(900)}`) }),
};
const versions = {};
for (const [k, body] of Object.entries(stored)) { versions[k] = ver(k); fs.writeFileSync(path.join(docs, versions[k] + '.json'), body); }
versions.missing = ver('missing');
const docFile = (available, extra = {}) => ({ name: `QA-${typeof available === "string" ? available.slice(0, 6) : typeof available}.pdf`, document: { state: 'partial', pages: 3, version: OTHER, availableVersion: available, indexing: 'ready', ...extra } });
const pageFiles = [
  ...Object.values(versions).map((v) => docFile(v)),
  docFile(undefined), docFile(''), docFile('ZZ'), docFile([versions.normal]), docFile({ toString: 1 }), docFile(5), docFile(versions.normal.toUpperCase()),
  docFile(versions.normal, { pageStatus: [null] }), { name: 'QA-nodoc.pdf' }, { name: 'QA-docstr.pdf', document: 'qa' }, null,
];
const ARGS = [{}, { start: 1 }, { start: 1, end: 3 }, { start: 1, end: 3, offset: 8000 }, { start: 2, offset: 7999 }, { start: 2, end: 2, offset: 7998 },
  { start: 1, end: 5 }, { start: 1, end: 6 }, { start: 3, end: 4 }, { start: 0 }, { start: 1, offset: -1 }, { start: '2' }, { start: 1.5 }, { start: 1, end: null },
  { start: null }, { start: 2, end: 1 }, { start: 1, end: 1, offset: 1e300 }, { start: 1, end: 3, offset: 9007199254740992 }, { start: 1, end: 3, cap: 10 },
  { start: 1, end: 2, offset: 3, cap: 1 }, { start: 1, end: 2, offset: 4, cap: 1 }, { start: 1, end: 2, offset: 5, cap: 2 }, { start: 1, end: 3, offset: 1, cap: 0 },
  { start: 4, end: 5 }, { start: 1, end: 3, offset: 0.5 }, { start: 1, offset: '0' }, { start: 3, end: 5, offset: 3000 }];
// Every argument set for the two rich results, a middle set for the other readable ones, two for the rest.
const MIDDLE = [{}, { start: 1, end: 3 }, { start: 2, end: 3, offset: 10, cap: 50 }, { start: 1, end: 5, cap: 40 }, { start: 3, end: 4 }, { start: 1, end: 2, offset: 3, cap: 1 },
  { start: 1, end: 2, offset: 4, cap: 1 }, { start: 2, end: 2, cap: 30 }];
const argsFor = (f) => {
  const v = f && f.document && f.document.availableVersion;
  if (v === versions.normal || v === versions.long) return f.document.pageStatus ? MIDDLE : ARGS;
  if (v === versions.hexPages) return [{ start: 144115188075855904 }, { start: 144115188075855900 }, { start: 3 }, { start: 1, end: 3 }];
  if ([versions.noPages, versions.lone, versions.stringy, versions.arrays, versions.dupPages].includes(v)) return MIDDLE;
  return [{ start: 1, end: 2 }, { start: 1, end: 9 }];
};
const pageCases = [];
for (const f of pageFiles) {
  const file = json(f) ?? null;
  for (const a of argsFor(f)) {
    const args = json(a);
    pageCases.push({ file, args, result: attempt(() => ds.readPages({ dir: dataDir }, PROJECT, file, args.start, args.end, args.offset, args.cap)) });
  }
}
fs.rmSync(dataDir, { recursive: true, force: true });

const out = JSON.stringify({ about: 'noevia-core document-sources.cjs answers on synthetic QA records (crates/document-read)', sources,
  project: PROJECT, stored, versions, files: fileCases, notes: noteCases, pages: pageCases }, null, 1) + '\n';
const check = process.argv.indexOf('--check');
if (check !== -1) {
  const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
  if (committed !== out) { process.stderr.write('The recording differs from the committed fixture.\n'); process.exit(1); }
  process.stderr.write('The recording matches the committed fixture.\n');
} else process.stdout.write(out);
