#!/usr/bin/env node
'use strict';
// Recorded document extractions of noevia-core (server/documents.cjs extractDocumentText and completeDocument,
// server/document-sources.cjs ingest) for crates/server-projects src/extract.rs and src/document_ingest.rs:
// tests/node_extraction.rs replays every scenario through the Rust code with the same scripted engine.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/server-projects/tests/record_extraction.cjs > crates/server-projects/tests/fixtures/node-extraction.v1.json
//   ... node crates/server-projects/tests/record_extraction.cjs --check crates/server-projects/tests/fixtures/node-extraction.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Nothing from node_modules; synthetic QA-named data only. Each scenario carries a script for the two sidecars:
//   docling  replies keyed by the request ("whole" or "<first>-<last>", "+figures" when figures are asked), each
//            key a queue consumed in order: { ok: <reply body> } or { error: { message, status?, permanent?,
//            code?, timeout? } } (an error as docling.cjs / ocr.cjs raise it; `timeout` is AbortSignal's)
//   ranges   what docling.cjs rangesSupported answers (true, false or null)
//   caps     what ocr.cjs capabilities answers (null, or { maxBytes?, dpis? })
//   ocr      replies keyed by "<pages joined by ,>@<dpi>" (a queue each): { ok: [pages] } or { error }
//   native   (one scenario only) what the pdf.js reader returns where core would use it; Rust has no such reader
// and flags { docling, ocr, multimodal }, the PROJECT_DOCUMENT_* env, and an OCR budget (a sync's) or null.
//   extract   documents.extractDocumentText(name, bytes, deps): the engine functions as deps, waits [0, 0]
//   complete  documents.completeDocument(name, bytes, input, deps) for a stored record `input`
//   ingest    document-sources.ingest(workspace, projectId, name, bytes, previous, { ocrBudget, onOutcome }) with
//             docling.cjs / ocr.cjs module functions replaced by the script and the flags set in process.env; a
//             `stored` result is written first at its version path. No busy (503) replies here: ingest cannot
//             shorten the busy waits.
// Recorded per scenario: Node's result as JSON text (figure bytes taken out and listed as figureBytes, length and
// sha256 in order), or what it threw; the ordered engine calls (bytes as length + sha256); the budget after; for
// ingest the returned source record, the stored result file and every file in the project's document folder.
// Each core source file's sha256 is recorded. Console output goes to stderr; stdout is the fixture.

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

// The environment the modules read at call time: cleared, then set per scenario.
const ENV_KEYS = ['DOCLING_BASE_URL', 'OCR_BASE_URL', 'NOEVIA_MULTIMODAL_RAG', 'PROJECT_DOCUMENT_READ_BYTES', 'PROJECT_DOCUMENT_PAGE_LIMIT',
  'PROJECT_DOCUMENT_TEXT_LIMIT', 'PROJECT_DOCUMENT_RANGE_PAGES', 'PROJECT_SYNC_OCR_PAGES', 'LARGE_DOCUMENT_ACQUIRE_WAIT_MS', 'LARGE_DOCUMENT_HOLD_WARN_MS'];
for (const k of ENV_KEYS) delete process.env[k];
// The host-wide lock file's folder does not exist, so large-document-file-lock.cjs takes no file (in-process only).
const tmp = path.join(os.tmpdir(), 'qa-extraction-r13');
fs.rmSync(tmp, { recursive: true, force: true });
process.env.UI_DATA_DIR = path.join(tmp, 'no-ui-data');

const SOURCE_FILES = ['server/documents.cjs', 'server/document-sources.cjs', 'server/docling.cjs', 'server/ocr.cjs', 'server/source-limits.cjs',
  'server/large-document-lock.cjs', 'server/large-document-file-lock.cjs', 'server/document-workers.cjs', 'server/multimodal-rag.cjs',
  'server/quoted-name.cjs', 'contracts/folder-import.json'];
const sources = Object.fromEntries(SOURCE_FILES.map((f) => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex')]));
const req = (f) => require(path.join(root, 'server', f));
const documents = req('documents.cjs');
const ds = req('document-sources.cjs');
const docling = req('docling.cjs');
const ocr = req('ocr.cjs');
const sourceLimits = req('source-limits.cjs');

const sha = (b) => crypto.createHash('sha256').update(b).digest('hex');
const clone = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));
const MiB = 1024 * 1024;
const LARGE = 25 * MiB + 4096; // over WHOLE_BYTES (25 MiB): read in ranges, under the large-document lock

/** The bytes of a scenario: { text } (UTF-8) or { repeat, length } (the pattern repeated to that length). */
function bytesOf(spec) {
  if (typeof spec.text === 'string') return Buffer.from(spec.text, 'utf8');
  return Buffer.alloc(spec.length, spec.repeat, 'utf8');
}
const pdf = (tag) => ({ text: `%PDF-1.7\n% QA synthetic document ${tag}\n` });
const large = (tag) => ({ repeat: `%PDF-1.7 QA large synthetic document ${tag} `, length: LARGE });

// ── reply builders ───────────────────────────────────────────────────────
const page = (number, text, status = 'native', extra = {}) => ({ number, text, status, method: 'docling', ...extra });
const reply = (total, pages, extra = {}) => ({ ok: { total, pages, ...extra } });
const ocrOk = (pages) => ({ ok: pages });
const busy = (what = 'OCR') => ({ error: { message: what === 'OCR' ? 'OCR is busy; refresh to retry.' : 'Document extraction is busy; refresh to retry.', status: 503 } });
const timeout = { error: { timeout: true } };
const fetchFailed = { error: { message: 'fetch failed' } };
const png = (tag) => Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), Buffer.from(`QA png ${tag}`)]).toString('base64');
const jpeg = (tag) => Buffer.concat([Buffer.from([0xff, 0xd8, 0xff, 0xe0]), Buffer.from(`QA jpeg ${tag}`)]).toString('base64');
const webp = (tag) => Buffer.concat([Buffer.from('RIFF'), Buffer.from([9, 0, 0, 0]), Buffer.from('WEBP'), Buffer.from(`QA webp ${tag}`)]).toString('base64');
const DPI = { dpis: [300, 400] };

const scenarios = [];
const add = (kind, name, s) => scenarios.push({ kind, name, parity: true, file: { name: 'QA-report.pdf', bytes: pdf(name) },
  flags: { docling: true, ocr: true, multimodal: false }, env: {}, budget: null, ...s, script: { ranges: null, caps: null, docling: {}, ocr: {}, ...s.script } });

// ── extractDocumentText: the first pass and its completion ───────────────
add('extract', 'pdf-native-ready', { script: { docling: { whole: [reply(3, [page(1, 'QA page one.'), page(2, 'QA page two.'), page(3, '', 'blank')])] } } });
add('extract', 'reply-shapes', { flags: { docling: true, ocr: false, multimodal: false }, script: { docling: { whole: [reply('3', [
  { number: '1', text: 'QA numbered as text', status: 'native', method: 'pdfium', reason: 'low-text' },
  { number: 2, text: 5, status: 'native', reason: 'Has Caps', truncated: 1 },
  { number: 3, text: '' },
], { truncatedPages: 2 })] } } });
add('extract', 'retry-recovered-400dpi', { script: { caps: DPI, docling: { whole: [reply(3, [page(1, 'QA page one.'), page(2, '', 'failed'),
  page(3, 'QA faint text', 'degraded', { reason: 'native-fallback' })])] },
  ocr: { '2,3@400': [ocrOk([{ number: 2, text: '  QA page two by OCR  ' }, { number: 3, text: 'QA page three by OCR', truncated: false }])] } } });
add('extract', 'retry-outcomes-batches', { script: { docling: { whole: [reply(6, [page(1, '', 'failed'), page(2, '', 'unreadable'), page(3, '', 'ocr-needed'),
  page(4, 'QA degraded four', 'degraded'), page(5, '', 'failed'), page(6, 'QA six')])] },
  ocr: { '1,2,3@400': [ocrOk([{ number: 1, text: 'QA one by OCR' }, { number: 2, text: '   ' }, { number: 3, error: 'QA page could not be rendered' }])],
    '4,5@400': [ocrOk([{ number: 4, text: 'QA four by OCR', truncated: true }])] } } });
add('extract', 'retry-unavailable-without-ocr', { flags: { docling: true, ocr: false, multimodal: false }, script: { docling: { whole: [reply(3, [
  page(1, 'QA kept', 'degraded'), page(2, '', 'failed'), page(3, 'QA fine')])] } } });
{
  const pages = [page(1, 'QA cover page')];
  for (let n = 2; n <= 24; n++) pages.push(page(n, '', 'failed'));
  const ocrScript = {};
  for (let first = 2; first <= 21; first += 3) {
    const batch = []; for (let n = first; n <= Math.min(first + 2, 21); n++) batch.push(n);
    ocrScript[`${batch.join(',')}@400`] = [ocrOk(batch.map((n) => ({ number: n, text: `QA page ${n} by OCR` })))];
  }
  add('extract', 'retry-cap-20-pages', { script: { caps: DPI, docling: { whole: [reply(24, pages)] }, ocr: ocrScript } });
}
add('extract', 'retry-busy-then-recovered', { script: { docling: { whole: [reply(2, [page(1, 'QA one'), page(2, '', 'failed')])] },
  ocr: { '2@400': [busy(), ocrOk([{ number: 2, text: 'QA two after a busy answer' }])] } } });
add('extract', 'retry-busy-exhausted', { script: { docling: { whole: [reply(2, [page(1, 'QA one'), page(2, '', 'failed')])] },
  ocr: { '2@400': [busy(), busy(), busy()] } } });
add('extract', 'retry-timeout', { script: { docling: { whole: [reply(2, [page(1, 'QA one'), page(2, '', 'ocr-needed')])] }, ocr: { '2@400': [timeout] } } });
add('extract', 'retry-final-error', { script: { docling: { whole: [reply(2, [page(1, 'QA one'), page(2, '', 'failed')])] },
  ocr: { '2@400': [{ error: { message: 'Invalid OCR response; refresh to retry.' } }] } } });
add('extract', 'retry-http-400-passing', { script: { docling: { whole: [reply(2, [page(1, 'QA one'), page(2, '', 'failed')])] },
  ocr: { '2@400': [{ error: { message: 'OCR unavailable (HTTP 400); refresh to retry.', status: 400 } }] } } });
add('extract', 'retry-text-limit', { env: { PROJECT_DOCUMENT_TEXT_LIMIT: '60' }, script: { docling: { whole: [reply(3, [
  page(1, 'QA page one holds most of the text allowance here.'), page(2, '', 'failed'), page(3, '', 'failed')])] },
  ocr: { '2,3@400': [ocrOk([{ number: 2, text: 'QA OCR text far longer than what is left' }, { number: 3, text: 'QA three' }])] } } });
add('extract', 'ranges-past-first-window', { env: { PROJECT_DOCUMENT_RANGE_PAGES: '2' }, script: { ranges: true, caps: DPI, docling: {
  whole: [reply(7, [page(1, 'QA one'), page(2, 'QA two'), page(3, 'QA three')])],
  '4-5': [reply(7, [page(4, 'QA four'), page(5, 'QA five', 'native', { method: 'pdfium' })])],
  '6-7': [reply(7, [page(6, 'QA six')])] },
  ocr: { '7@400': [ocrOk([{ number: 7, text: 'QA seven by OCR' }])] } } });
add('extract', 'page-limit-stop', { env: { PROJECT_DOCUMENT_PAGE_LIMIT: '5', PROJECT_DOCUMENT_RANGE_PAGES: '2' }, script: { ranges: true, docling: {
  whole: [reply(9, [page(1, 'QA one'), page(2, 'QA two'), page(3, 'QA three')])], '4-5': [reply(9, [page(4, 'QA four'), page(5, 'QA five')])] } } });
add('extract', 'text-limit-stop', { env: { PROJECT_DOCUMENT_TEXT_LIMIT: '40', PROJECT_DOCUMENT_RANGE_PAGES: '2' }, script: { ranges: true, docling: {
  whole: [reply(6, [page(1, 'QA first page text.'), page(2, 'QA two')])],
  '3-4': [reply(6, [page(3, 'QAx 😀😀😀😀😀😀😀😀😀😀'), page(4, 'QA four')])] } } });
add('extract', 'ranges-unasked-when-not-needed', { script: { ranges: true, docling: { whole: [reply(2, [page(1, 'QA one'), page(2, 'QA two')])] } } });
add('extract', 'range-busy-stop', { flags: { docling: true, ocr: false, multimodal: false }, env: { PROJECT_DOCUMENT_RANGE_PAGES: '2' }, script: { ranges: true, docling: {
  whole: [reply(4, [page(1, 'QA one'), page(2, 'QA two')])], '3-4': [busy('docling'), busy('docling'), busy('docling')] } } });
add('extract', 'range-final-error', { env: { PROJECT_DOCUMENT_RANGE_PAGES: '2' }, script: { ranges: true, docling: {
  whole: [reply(4, [page(1, 'QA one'), page(2, 'QA two')])],
  '3-4': [{ error: { message: 'This document could not be read; the original is stored.', permanent: true } }] } } });
add('extract', 'range-timeout', { env: { PROJECT_DOCUMENT_RANGE_PAGES: '2' }, script: { ranges: true, docling: {
  whole: [reply(4, [page(1, 'QA one'), page(2, 'QA two')])], '3-4': [timeout] } } });
add('extract', 'range-fetch-failed', { env: { PROJECT_DOCUMENT_RANGE_PAGES: '2' }, script: { ranges: true, docling: {
  whole: [reply(4, [page(1, 'QA one'), page(2, 'QA two')])], '3-4': [fetchFailed] } } });
add('extract', 'docling-fetch-failed', { script: { docling: { whole: [fetchFailed] } } });
add('extract', 'docling-busy', { script: { docling: { whole: [busy('docling')] } } });
add('extract', 'docling-refused-400', { script: { docling: { whole: [{ error: { message: 'noevia sent this document in a form the extractor refused; it will be read again after the next update.' } }] } } });
add('extract', 'docling-http-500', { script: { docling: { whole: [{ error: { message: 'Document extraction unavailable (HTTP 500); refresh to retry.' } }] } } });
add('extract', 'docling-timeout', { script: { docling: { whole: [timeout] } } });
add('extract', 'docling-422-permanent', { script: { docling: { whole: [{ error: { message: 'This document could not be read; the original is stored.', permanent: true } }] } } });
add('extract', 'docx-415-permanent', { file: { name: 'QA-letter.docx', bytes: { text: 'PK QA synthetic docx' } },
  script: { docling: { whole: [{ error: { message: 'This file type cannot be read yet; the original is stored.', permanent: true } }] } } });
add('extract', 'docx-completion-marker-only', { file: { name: 'QA-letter.docx', bytes: { text: 'PK QA synthetic docx' } }, script: { ranges: true, caps: DPI,
  docling: { whole: [reply(5, [page(1, 'QA letter page one'), page(2, '', 'failed')])] } } });
add('extract', 'html-completion-marker-only', { file: { name: 'Folder/QA-page.html', bytes: { text: '<html><body>QA page</body></html>' } },
  script: { docling: { whole: [reply(1, [page(1, 'QA page')])] } } });
add('extract', 'empty-reply', { script: { docling: { whole: [reply(0, [])] } } });
{
  const figures = [
    { page: 1, index: 0, mime: 'image/png', data: png('one'), caption: '  QA chart caption  ', text: ' QA chart text ' },
    { page: 3, index: 1, mime: 'image/jpeg', data: jpeg('three') },
    { page: 3, index: 2, mime: 'image/webp', data: webp('three'), caption: '   ' },
    { page: 3, index: 3, mime: 'image/png', data: jpeg('wrong magic') },
    { page: 2, index: 4, mime: 'image/png', data: png('scanned page') },
    { page: 9, index: 5, mime: 'image/png', data: png('no such page') },
    { page: 1, index: 1000, mime: 'image/png', data: png('index too big') },
    { page: 1, index: 6, mime: 'image/gif', data: png('gif') },
    { page: 1, index: 7, mime: 'image/png', data: 12 },
    { page: '1', index: '8', mime: 'image/png', data: png('numbers as text') },
  ];
  const pages = [page(1, 'QA one'), page(2, 'QA scanned two', 'native', { ocr: true }), page(3, 'QA three')];
  const env = { PROJECT_DOCUMENT_RANGE_PAGES: '2' };
  add('extract', 'figures-on', { flags: { docling: true, ocr: true, multimodal: true }, env, script: { ranges: true, docling: {
    'whole+figures': [reply(5, pages, { figures })],
    '4-5+figures': [reply(5, [page(4, 'QA four'), page(5, 'QA five')], { figures: [{ page: 4, index: 0, mime: 'image/png', data: png('range four') },
      { page: 2, index: 1, mime: 'image/png', data: png('outside this range') }] })] } } });
  add('extract', 'figures-off', { flags: { docling: true, ocr: true, multimodal: false }, env, script: { ranges: true, docling: {
    whole: [reply(5, pages, { figures })], '4-5': [reply(5, [page(4, 'QA four'), page(5, 'QA five')])] } } });
}
add('extract', 'large-first-range-ocr-too-small', { file: { name: 'QA-large.pdf', bytes: large('a') }, env: { PROJECT_DOCUMENT_RANGE_PAGES: '3' },
  script: { ranges: true, caps: {}, docling: { '1-3': [reply(5, [page(1, 'QA one'), page(2, '', 'failed'), page(3, 'QA three')], { truncatedPages: 2 })],
    '4-5': [reply(5, [page(4, 'QA four'), page(5, 'QA five')])] } } });
add('extract', 'large-ocr-takes-it', { file: { name: 'QA-large.pdf', bytes: large('b') }, env: { PROJECT_DOCUMENT_RANGE_PAGES: '3' },
  script: { ranges: true, caps: { maxBytes: 64 * MiB, dpis: [400] }, docling: { '1-3': [reply(3, [page(1, 'QA one'), page(2, '', 'failed'), page(3, 'QA three')])] },
    ocr: { '2@400': [ocrOk([{ number: 2, text: 'QA large two by OCR' }])] } } });
add('extract', 'ocr-budget-deferred', { budget: 2, script: { docling: { whole: [reply(3, [page(1, '', 'failed'), page(2, '', 'failed'), page(3, '', 'failed')])] },
  ocr: { '1,2@400': [ocrOk([{ number: 1, text: 'QA one by OCR' }, { number: 2, text: 'QA two by OCR' }])] } } });
add('extract', 'ocr-reply-number-as-text', { script: { docling: { whole: [reply(2, [page(1, 'QA one'), page(2, '', 'failed')])] },
  ocr: { '2@400': [ocrOk([{ number: '2', text: 'QA two keyed by a string' }])] } } });
// The one scenario core reads with its pdf.js reader (Docling not configured). Not a parity case: Rust has no
// in-process reader and stores the retryable "not configured" failure (extract.rs module docs).
add('extract', 'docling-not-configured-pdfjs', { parity: false, flags: { docling: false, ocr: false, multimodal: false },
  script: { native: { numPages: 2, pageTexts: [{ number: 1, text: 'QA native one', status: 'native', method: 'native' }, { number: 2, text: 'QA native two', status: 'native', method: 'native' }] } } });

// ── completeDocument on a stored record ──────────────────────────────────
const rec = (pageTexts, pages, extra = {}) => {
  const text = pageTexts.map((p) => `[Page ${p.number}]\n${p.text || '(' + p.status + ')'}`).join('\n\n');
  return { text, pages, pageTexts, truncated: pages > pageTexts.length, state: 'partial', retryable: false, ...extra };
};
add('complete', 'complete-attempts-exhausted', { input: rec([page(1, 'QA one'), page(2, '', 'failed', { retry: { outcome: 'failed', dpi: 400, error: 'fetch failed', attempts: 2, again: true } })], 2),
  script: { ocr: { '2@400': [fetchFailed] } } });
add('complete', 'complete-retry-again-recovered', { input: rec([page(1, 'QA one'), page(2, '', 'failed', { retry: { outcome: 'failed', dpi: 400, error: 'OCR is busy; refresh to retry.', attempts: 1, again: true } })], 2),
  script: { caps: DPI, ocr: { '2@400': [ocrOk([{ number: 2, text: 'QA two at last' }])] } } });
{
  const pages = [page(1, 'QA one')];
  for (let n = 2; n <= 19; n++) pages.push(page(n, '', 'failed', { retry: { outcome: 'empty', dpi: 400 } }));
  for (let n = 20; n <= 23; n++) pages.push(page(n, '', 'failed'));
  add('complete', 'complete-cap-counts-earlier-tries', { input: rec(pages, 23), script: { ocr: { '20,21@400': [ocrOk([{ number: 20, text: 'QA twenty' }, { number: 21, text: '' }])] } } });
}
add('complete', 'complete-range-attempts-exhausted', { flags: { docling: true, ocr: false, multimodal: false }, env: { PROJECT_DOCUMENT_RANGE_PAGES: '2' },
  input: rec([page(1, 'QA one'), page(2, 'QA two')], 4, { rangeAttempts: 2 }), script: { ranges: true, docling: { '3-4': [fetchFailed] } } });
add('complete', 'complete-range-resumed', { env: { PROJECT_DOCUMENT_RANGE_PAGES: '2' },
  input: rec([page(1, 'QA one'), page(2, 'QA two')], 4, { rangeAttempts: 1, error: 'QA earlier note', errorId: 'qaEarlier' }),
  script: { ranges: true, docling: { '3-4': [reply(4, [page(3, 'QA three'), page(4, 'QA four')])] } } });
add('complete', 'complete-already-completed', { input: rec([page(1, 'QA one'), page(2, '', 'failed')], 2, { completion: 'ranges-ocr-retry-v1' }) });
add('complete', 'complete-retryable-untouched', { input: { text: '', pages: 0, pageTexts: [], truncated: false, state: 'failed', retryable: true, error: 'fetch failed' } });
add('complete', 'complete-ocr-sent-not-finer', { input: rec([page(1, 'QA one'), page(2, '', 'unreadable', { ocrSent: true }), { number: 3, text: '', status: 'unreadable', method: 'native' }], 3),
  script: { caps: { dpis: [300] } } });
add('complete', 'complete-ocr-sent-finer', { input: rec([page(1, 'QA one'), page(2, '', 'unreadable', { ocrSent: true }), { number: 3, text: '', status: 'unreadable', method: 'native' }], 3),
  script: { caps: DPI, ocr: { '2,3@400': [ocrOk([{ number: 2, text: 'QA two finer' }, { number: 3, text: 'QA three finer' }])] } } });
add('complete', 'complete-size-blocked-due-again', { file: { name: 'QA-large.pdf', bytes: large('c') },
  input: rec([page(1, 'QA one'), page(2, '', 'failed', { retry: { outcome: 'failed', error: 'the PDF is larger than the OCR service accepts (25 MB)' } })], 2),
  script: { caps: { maxBytes: 64 * MiB, dpis: [400] }, ocr: { '2@400': [ocrOk([{ number: 2, text: 'QA two now the worker takes it' }])] } } });
add('complete', 'complete-gaps-filled', { input: rec([page(1, 'QA one'), page(3, 'QA three'), page(5, '', 'blank')], 5),
  script: { ocr: { '2,4@400': [ocrOk([{ number: 2, text: 'QA two' }, { number: 4, error: 'QA render failed' }])] } } });

// ── ingest: the source record and the stored files ───────────────────────
const prevDoc = (extra = {}) => ({ name: 'QA-report.pdf', content: 'QA text of the last readable version', document: { version: sha('qa-prev-version'),
  byteHash: sha('qa-prev-bytes'), state: 'ready', availableVersion: sha('qa-prev-available'), availableByteHash: sha('qa-prev-available-bytes'), indexing: 'ready', ...extra } });
add('ingest', 'ingest-new-ready', { script: { docling: { whole: [reply(2, [page(1, 'QA one'), page(2, 'QA two')])] } } });
add('ingest', 'ingest-new-partial-with-retries', { script: { caps: DPI, docling: { whole: [reply(4, [page(1, 'QA one'), page(2, '', 'failed'), page(3, 'QA dim', 'degraded', { reason: 'native-fallback' }), page(4, '', 'failed')])] },
  ocr: { '2,3,4@400': [ocrOk([{ number: 2, text: 'QA two by OCR' }, { number: 4, error: 'QA render failed' }])] } } });
add('ingest', 'ingest-new-figures', { flags: { docling: true, ocr: true, multimodal: true }, script: { docling: { 'whole+figures': [reply(2, [page(1, 'QA one'), page(2, 'QA two')], { figures: [
  { page: 1, index: 0, mime: 'image/png', data: png('first'), caption: 'QA first figure' },
  { page: 2, index: 0, mime: 'image/png', data: png('second, same index') },
  { page: 2, index: 1, mime: 'image/jpeg', data: jpeg('third'), text: 'QA figure text' }] })] } } });
add('ingest', 'ingest-failed-keeps-stale-text', { previous: prevDoc(), script: { docling: { whole: [fetchFailed] } } });
add('ingest', 'ingest-failed-without-previous', { script: { docling: { whole: [fetchFailed] } } });
add('ingest', 'ingest-permanent-no-previous', { script: { docling: { whole: [{ error: { message: 'This document could not be read; the original is stored.', permanent: true } }] } } });
add('ingest', 'ingest-permanent-with-previous', { previous: prevDoc({ indexing: 'indexed' }), script: { docling: { whole: [{ error: { message: 'This document could not be read; the original is stored.', permanent: true } }] } } });
add('ingest', 'ingest-cached-complete-unchanged', { stored: rec([page(1, 'QA stored one'), page(2, '', 'failed', { retry: { outcome: 'empty', dpi: 400 } })], 2, { completion: 'ranges-ocr-retry-v1' }) });
add('ingest', 'ingest-cached-completion-pass', { env: { PROJECT_DOCUMENT_RANGE_PAGES: '2' }, previous: prevDoc(),
  stored: rec([page(1, 'QA stored one'), page(2, 'QA stored two')], 4), script: { ranges: true, docling: { '3-4': [reply(4, [page(3, 'QA three'), page(4, 'QA four')])] } } });
add('ingest', 'ingest-cached-retryable-read-again', { stored: { text: '', pages: 0, pageTexts: [], truncated: false, state: 'failed', retryable: true, error: 'fetch failed' },
  script: { docling: { whole: [reply(1, [page(1, 'QA read again')])] } } });
add('ingest', 'ingest-same-available-version', { previous: 'same-version', script: { docling: { whole: [reply(1, [page(1, 'QA one')])] } } });
add('ingest', 'ingest-range-stop-and-budget', { budget: 1, env: { PROJECT_DOCUMENT_RANGE_PAGES: '2', PROJECT_DOCUMENT_PAGE_LIMIT: '3' }, script: { ranges: true, docling: {
  whole: [reply(5, [page(1, '', 'failed'), page(2, '', 'failed')])], '3-3': [reply(5, [page(3, 'QA three')])] },
  ocr: { '1@400': [ocrOk([{ number: 1, text: 'QA one by OCR' }])] } } });
add('ingest', 'ingest-size-blocked-reopened', { file: { name: 'QA-large.pdf', bytes: large('d') },
  stored: rec([page(1, 'QA one'), page(2, '', 'failed', { retry: { outcome: 'failed', error: 'the PDF is larger than the OCR service accepts (25 MB)' } })], 2, { completion: 'ranges-ocr-retry-v1' }),
  script: { caps: { maxBytes: 64 * MiB, dpis: [400] }, ocr: { '2@400': [ocrOk([{ number: 2, text: 'QA two now the worker takes it' }])] } } });

// ── the scripted engine ──────────────────────────────────────────────────
const byteRef = (b) => ({ length: b.length, sha256: sha(b) });
function toError(e) {
  if (e.timeout) { const err = new Error('The operation was aborted due to timeout'); err.name = 'TimeoutError'; return err; }
  return Object.assign(new Error(e.message), e.status !== undefined ? { status: e.status } : {}, e.permanent ? { permanent: true } : {}, e.code ? { code: e.code } : {});
}
let unscripted = [];
function engine(s, calls) {
  const queues = { docling: new Map(Object.entries(clone(s.script.docling))), ocr: new Map(Object.entries(clone(s.script.ocr))) };
  const take = (kind, key) => {
    const q = queues[kind].get(key);
    if (!q || !q.length) { unscripted.push(`${s.name}: ${kind} ${key}`); return { error: { message: `QA script has no ${kind} reply for ${key}` } }; }
    return q.shift();
  };
  return {
    extractDocument: async (name, bytes, options) => {
      const figures = !!options?.figures, pages = options?.pages ?? null;
      calls.push({ fn: 'docling', name, bytes: byteRef(bytes), figures, pages });
      const r = take('docling', `${pages ? `${pages[0]}-${pages[1]}` : 'whole'}${figures ? '+figures' : ''}`);
      if (r.error) throw toError(r.error);
      return r.ok;
    },
    rangesSupported: async (options) => { calls.push({ fn: 'ranges', bytes: options?.bytes ?? null }); return s.script.ranges; },
    capabilities: async () => { calls.push({ fn: 'caps' }); return clone(s.script.caps); },
    extractPages: async (bytes, pages, options = {}) => {
      const dpi = options.dpi ?? null;
      calls.push({ fn: 'ocr', bytes: byteRef(bytes), pages: [...pages], dpi, timeoutMs: options.timeoutMs ?? null });
      const r = take('ocr', `${pages.join(',')}@${dpi ?? 300}`);
      if (r.error) throw toError(r.error);
      return r.ok;
    },
    readPages: async (bytes, options) => { calls.push({ fn: 'readPages', bytes: byteRef(bytes), options }); return clone(s.script.native); },
  };
}

/** The result with figure bytes taken out: { json, figureBytes }. */
function publicResult(out) {
  const figureBytes = [];
  const copy = Array.isArray(out?.figures) ? { ...out, figures: out.figures.map(({ bytes, ...rest }) => { if (bytes) figureBytes.push(byteRef(bytes)); return rest; }) } : out;
  return { result: JSON.stringify(copy), figureBytes };
}
const budgetAfter = (b) => (b ? { remaining: b.remaining, deferredPages: b.deferredPages || 0, deferredDocuments: b.deferredDocuments || 0 } : null);
function setEnv(s) {
  for (const k of ENV_KEYS) delete process.env[k];
  if (s.flags.docling) process.env.DOCLING_BASE_URL = 'http://qa-docling.invalid';
  if (s.flags.ocr) process.env.OCR_BASE_URL = 'http://qa-ocr.invalid';
  if (s.flags.multimodal) process.env.NOEVIA_MULTIMODAL_RAG = '1';
  Object.assign(process.env, s.env);
}
function listFiles(dir) {
  if (!fs.existsSync(dir)) return [];
  return fs.readdirSync(dir).sort().map((name) => ({ name, ...byteRef(fs.readFileSync(path.join(dir, name))) }));
}

const PROJECT = 'qa-proj-r13-extraction';
async function run(s) {
  const bytes = bytesOf(s.file.bytes);
  const calls = [];
  const e = engine(s, calls);
  const budget = s.budget === null ? undefined : { remaining: s.budget };
  const limits = sourceLimits.documentLimits(s.env);
  const node = {};
  if (s.kind === 'extract' || s.kind === 'complete') {
    const deps = { ocrEnabled: s.flags.ocr, extractPages: e.extractPages, doclingEnabled: s.flags.docling, extractDocument: e.extractDocument,
      readPages: e.readPages, figures: s.flags.docling && s.flags.multimodal, env: { ...s.env }, rangesSupported: e.rangesSupported, waits: [0, 0],
      ocrBudget: budget, ocrCapabilities: e.capabilities };
    try {
      const out = s.kind === 'extract' ? await documents.extractDocumentText(s.file.name, bytes, deps) : await documents.completeDocument(s.file.name, bytes, clone(s.input), deps);
      Object.assign(node, publicResult(out));
    } catch (err) { node.threw = { status: err.status ?? null, message: String(err.message) }; }
  } else {
    if (JSON.stringify(s.script).includes('"status":503')) throw new Error(`${s.name}: ingest cannot shorten the busy waits; no 503 replies`);
    setEnv(s);
    const saved = { extractDocument: docling.extractDocument, rangesSupported: docling.rangesSupported, extractPages: ocr.extractPages, capabilities: ocr.capabilities };
    Object.assign(docling, { extractDocument: e.extractDocument, rangesSupported: e.rangesSupported });
    Object.assign(ocr, { extractPages: e.extractPages, capabilities: e.capabilities });
    try {
      const userDir = path.join(tmp, s.name, 'users', 'qa-r13');
      fs.mkdirSync(userDir, { recursive: true });
      const workspace = { dir: userDir };
      const version = sha(sha(bytes) + ':' + documents.EXTRACTOR_VERSION);
      const docDir = ds.directory(workspace, PROJECT);
      if (s.stored) {
        fs.mkdirSync(docDir, { recursive: true });
        fs.writeFileSync(path.join(docDir, `${version}.json`), JSON.stringify(s.stored));
      }
      if (s.previous === 'same-version') s.previous = { name: s.file.name, content: 'QA earlier text', document: { version, byteHash: sha(bytes), state: 'ready', availableVersion: version, availableByteHash: sha(bytes), indexing: 'indexed' } };
      let outcome = null;
      const record = await ds.ingest(workspace, PROJECT, s.file.name, bytes, clone(s.previous) ?? undefined, { ...(budget ? { ocrBudget: budget } : {}), onOutcome: (k) => { outcome = k; } });
      node.ingest = { version, extractor: documents.EXTRACTOR_VERSION, record: JSON.stringify(record), resultFile: fs.readFileSync(path.join(docDir, `${version}.json`), 'utf8'),
        files: listFiles(docDir), outcome };
    } finally { Object.assign(docling, { extractDocument: saved.extractDocument, rangesSupported: saved.rangesSupported }); Object.assign(ocr, { extractPages: saved.extractPages, capabilities: saved.capabilities }); }
    for (const k of ENV_KEYS) delete process.env[k];
  }
  node.calls = calls;
  node.budgetAfter = budgetAfter(budget);
  const { kind, name, parity, file, flags, env, budget: b, script, input, previous, stored } = s;
  return { kind, name, parity, file, flags, env, limits, budget: b, script, ...(input ? { input } : {}), ...(previous ? { previous } : {}),
    ...(stored ? { stored } : {}), node };
}

(async () => {
  const out = [];
  for (const s of scenarios) out.push(await run(s));
  fs.rmSync(tmp, { recursive: true, force: true });
  if (unscripted.length) { process.stderr.write(`Calls the scripts do not answer:\n${unscripted.join('\n')}\n`); process.exit(1); }
  const text = JSON.stringify({ about: 'noevia-core document extraction (documents.cjs, document-sources.cjs ingest) on scripted Docling/OCR sidecars, synthetic QA documents (crates/server-projects extract.rs, document_ingest.rs)',
    sources, scenarios: out }, null, 1) + '\n';
  const check = process.argv.indexOf('--check');
  if (check !== -1) {
    const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
    if (committed !== text) { process.stderr.write('The recording differs from the committed fixture.\n'); process.exit(1); }
    process.stderr.write('The recording matches the committed fixture.\n');
  } else process.stdout.write(text);
})().catch((err) => { process.stderr.write(`${err.stack || err}\n`); process.exit(1); });
