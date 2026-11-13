#!/usr/bin/env node
'use strict';
// Recorded knowledge-file retrieval of a project turn for crates/chat-prepare (slice P2b): the contract
// src/retrieval.rs is replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-prepare/tests/record_retrieval.cjs > crates/chat-prepare/tests/fixtures/node-retrieval.v1.json
//   ... node crates/chat-prepare/tests/record_retrieval.cjs --check crates/chat-prepare/tests/fixtures/node-retrieval.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// The checkout needs server/wasm/dav-parse.wasm (frameUntrusted is the Rust port in it); nothing from node_modules.
// Every expectation comes from noevia-core's own code; the inputs are synthetic and QA-named (a seeded PRNG plus hand-made
// edges), passed through JSON first so Node reads exactly what is recorded:
//   build         chat-sources.cjs buildSources(entries, files)
//   partial       source-coverage.cjs partialSources(files, placed)
//   warning       source-coverage.cjs coverageWarning(partial)
//   injected      rust-chat-native.cjs injectedSources({ injected, placed, readable, reply })
//   readable      instruction-skills.cjs sources(project) (kept indices)
//   diaryProject  multimodal-rag.cjs diaryProject(id)
//   chatProject   diary-extras.cjs chatProjectId(chatId)
//   turns         chat.cjs createChatHandler().handleChat with the front's native executor asking for the turn
//                 (nativeHandoff, the `skills` capability), the local default provider, no tools, chat framing off.
//                 The two seams are stubs that answer per case and log their arguments in call order:
//                 sourceJobs.settle (the bounded wait for the project's file jobs, #1441) and rag.filesContext (the
//                 retrieval). The retrieval stub keeps rag.cjs's contract: nothing readable answers null without a
//                 report, the reporting hooks are called inside a try (a hook that throws never breaks the prompt),
//                 onImages gets no images (#1726's image half is a later slice), and a case may make it throw (rag
//                 never does in production). getProject gives the stored record with source-text.cjs's accessors
//                 attached, the texts written where they live. Recorded: the seams' arguments, the meta event's
//                 sourcesNotReady, the `sources` event, and the `prepared` frame's messages, sources and
//                 skills.coverageWarning; or, for a turn the executor refuses (a Diary turn), the refusal and the
//                 messages of the model request; or what handleChat threw.
// Each source file's sha256 is recorded (and chat.cjs's ported blocks), so a changed Node file needs a fresh recording.
// Node's console output goes to stderr; stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { EventEmitter } = require('node:events');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
const logged = [];
for (const k of ['log', 'info', 'warn', 'error', 'debug']) {
  console[k] = (...a) => { const line = a.map(String).join(' '); logged.push(line); process.stderr.write(`${line}\n`); };
}
const SOURCE_FILES = ['server/chat.cjs', 'server/chat-sources.cjs', 'server/source-coverage.cjs', 'server/source-text.cjs',
  'server/source-readability.cjs', 'server/instruction-skills.cjs', 'server/rust-chat-native.cjs', 'server/multimodal-rag.cjs',
  'server/diary-extras.cjs', 'server/quoted-name.cjs', 'server/rag.cjs', 'server/rust-source-ops.cjs', 'server/tools-mode.cjs',
  'server/wasm/dav-parse.wasm'];
const sources = Object.fromEntries(SOURCE_FILES.map((f) => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex')]));
const req = (f) => require(path.join(root, 'server', f));
const chatSources = req('chat-sources.cjs');
const coverage = req('source-coverage.cjs');
const native = req('rust-chat-native.cjs');
const skills = req('instruction-skills.cjs');
const multimodalRag = req('multimodal-rag.cjs');
const diaryExtras = req('diary-extras.cjs');
const sourceText = req('source-text.cjs');
const toolsMode = req('tools-mode.cjs');
const { createChatHandler } = req('chat.cjs');

// ── chat.cjs's ported lines ─────────────────────────────────────────────────────────────────
const chatText = fs.readFileSync(path.join(root, 'server/chat.cjs'), 'utf8');
const blocks = {};
function cut(name, start, end) {
  const once = (needle) => {
    const at = chatText.indexOf(needle);
    if (at < 0 || chatText.indexOf(needle, at + 1) >= 0) throw new Error(`chat.cjs holds ${JSON.stringify(needle.slice(0, 60))} ${at < 0 ? 'nowhere' : 'twice'}`);
    return at;
  };
  const from = once(start), to = once(end) + end.length;
  blocks[name] = { lines: [chatText.slice(0, from).split('\n').length, chatText.slice(0, to).split('\n').length - 1],
    sha256: crypto.createHash('sha256').update(chatText.slice(from, to)).digest('hex') };
}
cut('projectWait', '    let projectId = body.projectId || null;\n', "attached file(s) not ready for this turn`);\n    }\n");
cut('retrieval', '    // Project knowledge files: RAG retrieval replaces whole-file pasting (step 10).\n',
  '        multimodal ? { attached: chatAttachments, images: true, onImages: (found) => { imageRetrieval = found; } } : { attached: chatAttachments });\n    }\n');
cut('sourcesEvent', '    if (replySources.length && !body.compactOnly) send(', "{ type: 'sources', sources: replySources });\n");
cut('injectedRefusal', "          injected: injectedEntries.some((e) => e && e.kind === 'file'", "typeof e.file !== 'string'),\n");
cut('frameSources', '          sources: native.injectedSources({ injected: injectedEntries', 'reply: replySources }),\n');
cut('coverageWarning', '            revokedInHistory: [...revokedInHistory], imageWarning, coverageWarning: ', "coverageWarning(partialReads) } : null,\n");

const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-chat-retrieval-'));
process.on('exit', () => fs.rmSync(tmp, { recursive: true, force: true }));
function writeTexts(dir, projectId, texts) {
  const out = {};
  for (const text of texts || []) {
    const ref = sourceText.refOf(text);
    const at = sourceText.projectDir(dir, projectId);
    fs.mkdirSync(at, { recursive: true });
    fs.writeFileSync(path.join(at, `${ref}.txt`), text, 'utf8');
    out[ref] = text;
  }
  return out;
}

// ── Deterministic inputs ────────────────────────────────────────────────────────────────────
let seed = 0x2026_1113;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const pick = (list) => list[Math.floor(rand() * list.length)];
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));
const attempt = (f) => { try { return { out: f() }; } catch (e) { return { throws: e.constructor.name }; } };
// Text pieces: JS whitespace of every kind, characters \s does not match, astral characters and lone surrogates
// (never U+DBFE/U+DBFF, which the frame reader refuses), and markers.
const PIECES = ['QA', 'notes', 'ORCHID-42', 'INV-2042', '34.95', ' ', '  ', '\t', '\n', '\r\n', '\u000b', '\u000c', ' ', ' ',
  ' ', ' ', ' ', ' ', ' ', ' ', '　', '﻿', '\u0085', '᠎', '​', 'é', '日本語', '😀', '𝒳',
  '\ud83d', '\ude00', '</untrusted>', '$&', '[Page 3]', '…'];
const textOf = (n) => Array.from({ length: n }, () => pick(PIECES)).join('');
const NAMES = ['QA notes.txt', 'QA big.txt', 'QA/sub/report.md', 'QA "quoted".txt', 'QA 😀 astral.md', 'QA \ud83d lone.txt', 'QA 日本語.pdf',
  'QA image.png', 'qa notes.txt', '', ' ', 'x'.repeat(320), `${'y'.repeat(299)}😀`];
const PAGES = ['[Page 4]\nQA page text', '  \n[Page 12]\nQA', '[Page 0]\nQA', '[Page 1]', '[Page 100000]\nQA', '[Page 100001]\nQA',
  '[Page 123456]\nQA', '[Page 1234567]\nQA', '[Page 4] QA', '[Page 04]\nQA', '[Page ４]\nQA', '　﻿[Page 9]\nQA', 'QA [Page 5]\n',
  `${' '.repeat(31)}[Page 7]`, `${' '.repeat(32)}[Page 7]\nQA`, '[page 3]\nQA', '[Page 2]\r\nQA'];
const SCORES = [0.12345, 0.0005, -0.0005, 1.0005, 0.9999, 1e-9, 123.4565, 2.5e-4, 1e21, -1e21, 0, 0.5, -2.5, 0.1235, 7, 1.23e-7, 4.0004999];
const BIG = 'QA big source text. '.repeat(200); // 4000 units: over coverage's WHOLE_MAX

function bodyOf() {
  const r = rand();
  if (r < 0.1) return pick(PAGES);
  if (r < 0.15) return pick([null, 0, 5, true, false, [], ['QA', 'b'], {}, '']);
  if (r < 0.2) return 'z'.repeat(int(150, 260)) + textOf(int(0, 3));
  if (r < 0.25) return 'b'.repeat(int(1, 3400));
  return textOf(int(0, 40));
}
// `big`: some files are longer than coverage's WHOLE_MAX (only partialSources and whole turns read the length).
function fileOf(i, names = NAMES, big = false) {
  if (rand() < 0.04) return pick(['QA string file', 7, [], ['qa'], true, 0]);
  const f = {};
  if (rand() < 0.95) f.name = rand() < 0.85 ? pick(names) : pick([5, null, ['QA', 'b'], {}, true, `QA file-${i}.txt`]);
  if (rand() < 0.85) f.content = big && rand() < 0.3 ? 'p'.repeat(int(2300, 3400)) : rand() < 0.1 ? pick([0, 5, null, true, [], ['QA'], {}]) : textOf(int(0, 30));
  if (rand() < 0.4) {
    f.attachment = rand() < 0.85 ? { state: pick(['ready', 'stored', 'failed']), group: pick(['Text', 'Documents', 'Images', 'Other']) } : pick([null, 'stored', 5, []]);
    if (f.attachment && typeof f.attachment === 'object' && !Array.isArray(f.attachment) && rand() < 0.7) {
      f.attachment.id = rand() < 0.8 ? `qa-attachment-${i}` : pick([0, 5, '', null, true, ['a', 'b'], {}, 'w'.repeat(320)]);
    }
  }
  if (rand() < 0.2) f.document = { state: pick(['ready', 'failed', 'queued', 'reading']) };
  return f;
}
function entryOf(names) {
  if (rand() < 0.05) return pick([null, 0, 'QA string entry', 5, [], true]);
  const e = {};
  if (rand() < 0.95) e.file = rand() < 0.85 ? pick(names) : pick(['QA ghost.txt', 5, null, ['QA'], {}]);
  if (rand() < 0.95) e.body = bodyOf();
  if (rand() < 0.5) e.score = rand() < 0.8 ? pick(SCORES) : pick(['0.5', null, true, [], {}]);
  if (rand() < 0.95) e.kind = rand() < 0.9 ? pick(['excerpt', 'file']) : pick(['Excerpt', '', null, 5]);
  return e;
}

// ── buildSources ────────────────────────────────────────────────────────────────────────────
const buildCases = [];
function addBuild(name, rawEntries, rawFiles) {
  const entries = viaJson(rawEntries), files = viaJson(rawFiles);
  buildCases.push({ name, entries, files, sources: attempt(() => chatSources.buildSources(entries, files)) });
}
const plain = (name, extra = {}) => ({ name, content: 'QA text', ...extra });
addBuild('nothing', [], []);
addBuild('entries_not_list', { file: 'QA notes.txt' }, [plain('QA notes.txt')]);
addBuild('files_not_list', [{ file: 'QA notes.txt', body: 'QA' }], { 0: plain('QA notes.txt') });
addBuild('thirteen_cut_at_twelve', Array.from({ length: 13 }, (_, i) => ({ file: `QA ${i}.txt`, body: `QA ${i}`, kind: 'file' })),
  Array.from({ length: 13 }, (_, i) => plain(`QA ${i}.txt`)));
addBuild('last_name_wins', [{ file: 'QA dup.txt', body: 'QA', kind: 'file' }],
  [plain('QA dup.txt', { attachment: { id: 'qa-first' } }), plain('QA dup.txt', { attachment: { id: 'qa-second' } })]);
addBuild('unreadable_dropped', [{ file: 'QA stored.txt', body: 'QA' }, { file: 'QA failed.pdf', body: 'QA' }],
  [{ name: 'QA stored.txt', attachment: { state: 'stored', group: 'Text' } }, { name: 'QA failed.pdf', document: { state: 'failed' }, content: '' }]);
addBuild('pages', PAGES.map((body) => ({ file: 'QA doc.pdf', body, kind: 'excerpt' })).slice(0, 12), [plain('QA doc.pdf')]);
addBuild('pages_rest', PAGES.map((body) => ({ file: 'QA doc.pdf', body, kind: 'excerpt' })).slice(12), [plain('QA doc.pdf')]);
addBuild('pages_on_files', PAGES.slice(0, 4).map((body) => ({ file: 'QA doc.pdf', body, kind: 'file' })), [plain('QA doc.pdf')]);
addBuild('scores', SCORES.slice(0, 12).map((score) => ({ file: 'QA s.txt', body: 'QA', score, kind: 'excerpt' })), [plain('QA s.txt')]);
addBuild('scores_rest', SCORES.slice(12).map((score) => ({ file: 'QA s.txt', body: 'QA', score })), [plain('QA s.txt')]);
addBuild('snippets', [
  { file: 'QA a.txt', body: `  QA \t\n spaced 　﻿ out  ` }, { file: 'QA a.txt', body: 'w'.repeat(200) }, { file: 'QA a.txt', body: 'w'.repeat(201) },
  { file: 'QA a.txt', body: `${'w'.repeat(198)}😀tail` }, { file: 'QA a.txt', body: `${'w'.repeat(197)}     tail` },
  { file: 'QA a.txt', body: `${'w'.repeat(199)}\ud83dtail` }, { file: 'QA a.txt', body: 'QA\u0085᠎​kept' }, { file: 'QA a.txt' },
], [plain('QA a.txt')]);
addBuild('ids', [{ file: 'QA a.txt', body: 'QA' }], [plain('QA a.txt', { attachment: { id: 5 } })]);
addBuild('id_long', [{ file: 'QA a.txt', body: 'QA' }], [plain('QA a.txt', { attachment: { id: `${'i'.repeat(299)}😀` } })]);
addBuild('id_object', [{ file: 'QA a.txt', body: 'QA' }], [plain('QA a.txt', { attachment: { id: { qa: 1 } } })]);
addBuild('id_array', [{ file: 'QA a.txt', body: 'QA' }], [plain('QA a.txt', { attachment: { id: ['qa', 'b'] } })]);
addBuild('attachment_string', [{ file: 'QA a.txt', body: 'QA' }], [plain('QA a.txt', { attachment: 'stored' })]);
addBuild('id_throws', [{ file: 'QA a.txt', body: 'QA' }], [plain('QA a.txt', { attachment: { id: { toString: 1 } } })]);
addBuild('body_throws', [{ file: 'QA a.txt', body: { toString: 1 } }], [plain('QA a.txt')]);
addBuild('body_throws_unread', [{ file: 'QA ghost.txt', body: { toString: 1 } }], [plain('QA a.txt')]);
addBuild('content_throws', [{ file: 'QA a.txt', body: 'QA' }], [{ name: 'QA a.txt', content: { toString: 1 }, document: { state: 'failed' } }]);
addBuild('name_kinds', [{ file: 5, body: 'QA' }, { file: null, body: 'QA' }, { body: 'QA' }], [{ name: 5, content: 'QA' }, { content: 'QA' }, null, 7]);
for (let n = 0; n < 220; n += 1) {
  const files = Array.from({ length: int(0, 10) }, (_, i) => fileOf(i));
  const names = files.map((f) => f && f.name).filter((x) => x !== undefined);
  addBuild(`random_${n}`, Array.from({ length: int(0, 16) }, () => entryOf(names.length && rand() < 0.9 ? names : NAMES)), files);
}

// ── partialSources ──────────────────────────────────────────────────────────────────────────
const partialCases = [];
// The files get source-text.cjs's accessors as getProject's do (attachFile), with the given texts written where they
// live, so a source whose text is in its own file is measured as in a turn.
function addPartial(name, rawFiles, rawPlaced, rawTexts = []) {
  const stored = viaJson(rawFiles), files = viaJson(rawFiles), placed = viaJson(rawPlaced);
  const dir = fs.mkdtempSync(path.join(tmp, 'qa-partial-'));
  const texts = writeTexts(dir, 'qa-partial', rawTexts);
  if (Array.isArray(files)) for (const f of files) sourceText.attachFile(f, dir, 'qa-partial');
  partialCases.push({ name, files: stored, texts, placed, partial: attempt(() => coverage.partialSources(files, placed)) });
}
addPartial('nothing', [], []);
addPartial('files_not_list', { 0: plain('QA big.txt', { content: BIG }) }, []);
addPartial('placed_not_list', [plain('QA big.txt', { content: BIG })], null);
addPartial('none_read', [plain('QA big.txt', { content: BIG })], []);
addPartial('some_read', [plain('QA big.txt', { content: BIG })], [{ file: 'QA big.txt', body: 'x'.repeat(1234) }, { file: 'QA big.txt', body: 'y'.repeat(10) }]);
addPartial('whole_read', [plain('QA big.txt', { content: BIG })], [{ file: 'QA big.txt', body: BIG }]);
addPartial('over_read', [plain('QA big.txt', { content: BIG })], [{ file: 'QA big.txt', body: BIG }, { file: 'QA big.txt', body: 'more' }]);
addPartial('at_whole_max', [plain('QA edge.txt', { content: 'e'.repeat(2400) }), plain('QA over.txt', { content: 'e'.repeat(2401) })], []);
addPartial('astral_counts_two', [plain('QA astral.txt', { content: '😀'.repeat(1201) })], [{ file: 'QA astral.txt', body: '😀' }]);
addPartial('content_kinds', [{ name: 'QA n', content: 123456 }, { name: 'QA arr', content: ['x'.repeat(2500)] }, { name: 'QA obj', content: {} },
  { name: 'QA true', content: true }, 'QA string file', 7, null], [{ body: 'QA' }]);
addPartial('unnamed_matches_unnamed', [{ content: BIG }], [{ body: 'x'.repeat(100) }, 5, 'QA', true, { file: null, body: 'n' }]);
addPartial('null_name_matches_null_file', [{ name: null, content: BIG }], [{ file: null, body: 'x'.repeat(100) }, { body: 'u' }]);
addPartial('number_names', [{ name: 5, content: BIG }], [{ file: 5, body: 'x'.repeat(100) }, { file: '5', body: 'y' }]);
addPartial('body_kinds', [plain('QA big.txt', { content: BIG })], [{ file: 'QA big.txt', body: 12345 }, { file: 'QA big.txt', body: ['ab', 'c'] },
  { file: 'QA big.txt', body: {} }, { file: 'QA big.txt', body: null }, { file: 'QA big.txt', body: false }, { file: 'QA big.txt', body: true }]);
addPartial('body_throws', [plain('QA big.txt', { content: BIG })], [{ file: 'QA big.txt', body: { toString: 1 } }]);
addPartial('body_throws_unmatched', [plain('QA big.txt', { content: BIG })], [{ file: 'QA other.txt', body: { toString: 1 } }]);
addPartial('name_throws', [{ name: { toString: 1 }, content: BIG }], []);
addPartial('content_throws', [{ name: 'QA', content: { toString: 1 } }], []);
// Sources whose text is in its own file (#1806): present and missing texts, an own content that is "", chars of every
// kind (textLength takes a safe integer only), an uppercase pointer (not one) and a one-item list of a pointer (String()).
const PART_TEXT = `QA part text. ${'t'.repeat(5000)}`;
const PART_REF = sourceText.refOf(PART_TEXT);
const MISSING_REF = 'd'.repeat(64);
const ext = (name, extra) => ({ name, textRef: PART_REF, chars: PART_TEXT.length, ...extra });
addPartial('external_present', [ext('QA ext.txt')], [{ file: 'QA ext.txt', body: 'x'.repeat(300) }], [PART_TEXT]);
addPartial('external_present_whole', [ext('QA ext.txt')], [{ file: 'QA ext.txt', body: PART_TEXT }], [PART_TEXT]);
addPartial('external_missing', [ext('QA gone.txt', { textRef: MISSING_REF, chars: 7000 })], [{ file: 'QA gone.txt', body: 'x'.repeat(10) }]);
addPartial('external_stale_chars', [ext('QA stale.txt', { chars: 100 }), ext('QA stale big.txt', { chars: 90000 })], [], [PART_TEXT]);
addPartial('own_content_empty', [ext('QA inline.txt', { content: '', chars: 9000 })], [], [PART_TEXT]);
addPartial('own_content_text', [ext('QA inline.txt', { content: 'i'.repeat(3000), chars: 9000 })], [], [PART_TEXT]);
for (const chars of [-5, 0, 2400, 2401, 5000.5, 9007199254740992, 1e21, '5000', null, true, [5000], {}]) {
  addPartial(`chars_${JSON.stringify(chars)}_present`, [ext('QA c.txt', { chars })], [{ file: 'QA c.txt', body: 'x'.repeat(50) }], [PART_TEXT]);
  addPartial(`chars_${JSON.stringify(chars)}_missing`, [ext('QA c.txt', { textRef: MISSING_REF, chars })], []);
}
addPartial('no_chars', [{ name: 'QA n.txt', textRef: PART_REF }], [], [PART_TEXT]);
addPartial('uppercase_ref', [ext('QA upper.txt', { textRef: PART_REF.toUpperCase(), chars: 9000 })], [], [PART_TEXT]);
addPartial('ref_too_short', [ext('QA short.txt', { textRef: PART_REF.slice(1), chars: 9000 })], [], [PART_TEXT]);
addPartial('ref_list', [ext('QA list.txt', { textRef: [PART_REF], chars: 6000 })], [{ file: 'QA list.txt', body: 'x'.repeat(20) }], [PART_TEXT]);
addPartial('ref_list_no_chars', [ext('QA list.txt', { textRef: [PART_REF], chars: undefined })], [], [PART_TEXT]);
addPartial('ref_list_two', [ext('QA list2.txt', { textRef: [PART_REF, PART_REF], chars: 6000 })], [], [PART_TEXT]);
addPartial('ref_number', [ext('QA num.txt', { textRef: 5, chars: 6000 })], [], [PART_TEXT]);
for (let n = 0; n < 170; n += 1) {
  const files = Array.from({ length: int(0, 8) }, (_, i) => fileOf(i, NAMES, true));
  const names = files.map((f) => f && f.name).filter((x) => x !== undefined);
  const placed = Array.from({ length: int(0, 10) }, () => entryOf(names.length && rand() < 0.9 ? names : NAMES));
  addPartial(`random_${n}`, files, rand() < 0.05 ? pick([null, {}, 'QA']) : placed);
}

// ── coverageWarning ─────────────────────────────────────────────────────────────────────────
const warningCases = [];
function addWarning(name, partial) { warningCases.push({ name, partial, warning: coverage.coverageWarning(viaJson(partial)) }); }
addWarning('empty', []);
addWarning('not_list', null);
addWarning('one', [{ name: 'QA big.txt', length: 4000, read: 0 }]);
addWarning('raw_names', [{ name: 'QA "x"\n‮.txt', length: 1234567, read: 2400 }, { name: '😀\ud83d', length: 2401, read: 1 }]);
for (const n of [2401, 9999, 10000, 999999, 1000000, 123456789, 9007199254740991]) addWarning(`number_${n}`, [{ name: 'QA n', length: n, read: n - 1 }]);
for (let n = 0; n < 60; n += 1) {
  addWarning(`random_${n}`, Array.from({ length: int(1, 5) }, () => {
    const length = int(2401, rand() < 0.2 ? 5e8 : 50000);
    return { name: rand() < 0.7 ? pick(NAMES) : textOf(int(0, 6)), length, read: rand() < 0.3 ? 0 : int(0, length - 1) };
  }));
}

// ── injectedSources ─────────────────────────────────────────────────────────────────────────
const injectedCases = [];
function addInjected(name, raw) {
  const input = viaJson(raw);
  injectedCases.push({ name, input, sources: attempt(() => native.injectedSources(input)) });
}
addInjected('nothing', { injected: [], placed: [], readable: [], reply: [] });
addInjected('not_lists', { injected: null, placed: 'QA', readable: [], reply: {} });
addInjected('placed_skipped', { injected: [{ file: 'QA a.txt', body: 'QA a', kind: 'file' }, { file: 'QA b.txt', body: 'QA b', kind: 'file' }],
  placed: [{ file: 'QA a.txt', body: 'QA a', kind: 'file' }], readable: [plain('QA a.txt'), plain('QA b.txt', { attachment: { id: 'qa-b' } })], reply: [{ id: 'qa-a' }] });
addInjected('excerpts_skipped', { injected: [{ file: 'QA a.txt', body: 'QA', kind: 'excerpt' }, null, 5, { file: 'QA a.txt', body: 'QA' }], placed: [], readable: [plain('QA a.txt')], reply: [] });
addInjected('not_readable_item_null', { injected: [{ file: 'QA ghost.txt', body: 'QA', kind: 'file' }], placed: [], readable: [plain('QA a.txt')], reply: [] });
addInjected('text_cut', { injected: [{ file: 'QA a.txt', body: `${'t'.repeat(23999)}😀tail`, kind: 'file' }, { file: 'QA b.txt', body: 'u'.repeat(30000), kind: 'file' }],
  placed: [], readable: [plain('QA a.txt'), plain('QA b.txt')], reply: [] });
addInjected('body_kinds', { injected: [{ file: 'QA a.txt', body: 0, kind: 'file' }, { file: 'QA b.txt', body: ['x', 'y'], kind: 'file' }, { file: 'QA c.txt', kind: 'file' }],
  placed: [], readable: [plain('QA a.txt'), plain('QA b.txt'), plain('QA c.txt')], reply: [] });
addInjected('duplicates_kept', { injected: [{ file: 'QA a.txt', body: 'one', kind: 'file' }, { file: 'QA a.txt', body: 'two', kind: 'file' }], placed: [], readable: [plain('QA a.txt')], reply: [] });
addInjected('placed_null_entry', { injected: [{ file: 'QA a.txt', body: 'one', kind: 'file' }], placed: [null, { file: 5 }], readable: [plain('QA a.txt')], reply: [] });
addInjected('body_throws', { injected: [{ file: 'QA a.txt', body: { toString: 1 }, kind: 'file' }], placed: [], readable: [plain('QA a.txt')], reply: [] });
for (let n = 0; n < 200; n += 1) {
  const readable = Array.from({ length: int(0, 8) }, (_, i) => fileOf(i));
  const names = readable.map((f) => f && f.name).filter((x) => typeof x === 'string');
  const pool = names.length && rand() < 0.9 ? names : NAMES;
  const injected = Array.from({ length: int(0, 12) }, () => { const e = entryOf(pool); if (e && typeof e === 'object' && rand() < 0.6) e.kind = 'file'; return e; })
    .map((e) => (e && typeof e === 'object' && e.kind === 'file' && typeof e.file !== 'string' ? { ...e, file: pick(pool) } : e));
  const placed = injected.filter((e) => e && rand() < 0.3);
  const reply = (() => { const r = attempt(() => chatSources.buildSources(placed, readable)); return 'throws' in r ? [] : r.out; })();
  addInjected(`random_${n}`, { injected, placed, readable, reply });
}

// ── instruction-skills.cjs sources(project): the files retrieval is given ────────────────────
const readableCases = [];
function addReadable(name, rawProject) {
  const project = viaJson(rawProject);
  const list = Array.isArray(project.files) ? project.files : [];
  const r = attempt(() => skills.sources(project));
  let kept = null;
  if (!('throws' in r)) {
    let at = 0;
    kept = r.out.map((f) => { while (list[at] !== f) at += 1; at += 1; return at - 1; });
  }
  readableCases.push({ name, project, readable: kept, ...('throws' in r ? { throws: r.throws } : {}) });
}
const SKILL = (name, extra = {}) => ({ name, content: '---\nname: QA skill\ndescription: QA does things\n---\nQA body', ...extra });
addReadable('no_files', { id: 'qa-p' });
addReadable('plain', { id: 'qa-p', files: [plain('QA a.txt'), plain('QA b.txt')] });
addReadable('skills_out', { id: 'qa-p', files: [plain('QA a.txt'), SKILL('QA/SKILL.md'), SKILL('QA front.md'), { name: 'qa/skill.MD', content: 'QA' },
  { name: 'QA header.txt', content: '---\ndescription: x\n---\n' }, plain('QA tail.txt')] });
addReadable('selection_out', { id: 'qa-p', instructionSkills: { 'QA chosen.txt': { enabled: true } }, files: [plain('QA chosen.txt'), plain('QA other.txt')] });
addReadable('unreadable_out', { id: 'qa-p', files: [{ name: 'QA stored.txt', attachment: { state: 'stored', group: 'Text' } }, { name: 'QA failed.pdf', document: { state: 'failed' } },
  { name: 'QA queued.pdf', document: { state: 'queued' } }, { name: 'QA image.png', attachment: { state: 'stored', group: 'Images' } }, plain('QA ok.txt')] });
addReadable('primitives', { id: 'qa-p', files: ['QA string', 7, [], true, plain('QA ok.txt'), 7] });
addReadable('null_file_throws', { id: 'qa-p', files: [plain('QA ok.txt'), null] });
addReadable('content_throws', { id: 'qa-p', files: [{ name: 'QA x.txt', content: { toString: 1 } }] });
for (let n = 0; n < 120; n += 1) {
  const files = Array.from({ length: int(0, 10) }, (_, i) => (rand() < 0.15 ? SKILL(pick(['QA/SKILL.md', `QA skill ${i}.md`, 'QA x.txt'])) : fileOf(i)))
    .map((f) => (f && typeof f === 'object' && !Array.isArray(f) && f.content && typeof f.content === 'object' && !Array.isArray(f.content) ? { ...f, content: 'QA' } : f));
  addReadable(`random_${n}`, { id: `qa-p-${n}`, files, ...(rand() < 0.2 ? { instructionSkills: { [pick(NAMES)]: { enabled: rand() < 0.5 } } } : {}) });
}

// ── diaryProject / chatProjectId ────────────────────────────────────────────────────────────
const IDS = ['cowork-diary-extras', 'cowork-diary-extras-2', 'cowork-chat-context-diary-1', 'cowork-chat-context-diary_1', 'cowork-chat-context-diary1',
  'COWORK-CHAT-CONTEXT-DIARY-x', 'Cowork-Chat-Context-Diary_x', 'cowork-chat-context-diary', 'cowork-chat-context-qa', 'x-cowork-chat-context-diary-1',
  'cowork-chat-context-dıary-1', 'cowork-chat-context-ſiary-1', 'cowork-chat-context-diary-😀', '', 'qa-project', null, 0, 5, true, false, [], ['cowork-diary-extras'], {}];
const diaryProjectCases = IDS.map((id) => ({ id, diary: attempt(() => multimodalRag.diaryProject(viaJson(id))) }));
diaryProjectCases.push({ id: { toString: 1 }, diary: attempt(() => multimodalRag.diaryProject({ toString: 1 })) });
const CHAT_IDS = ['qa-chat-1', 'QA_chat', 'diary-qa-1', 'a', 'x'.repeat(100), 'x'.repeat(101), '', 'qa chat', 'qa.chat', 'qa/chat', 'qa-😀', 'qa\n', 5, null, true, [], {}, ['qa']];
const chatProjectCases = CHAT_IDS.map((chatId) => ({ chatId, id: diaryExtras.chatProjectId(viaJson(chatId)) }));

// ── Whole turns through handleChat ──────────────────────────────────────────────────────────
const TOOL_NAMES = ['qa_web_lookup', 'read_project_file', 'qa_unknown'];
const BOXES = [{ id: 'web-search', tools: [{ function: { name: 'qa_web_lookup' } }] }];
const turnTools = {
  public: TOOL_NAMES.filter((n) => toolsMode.isPublicToolName(n, BOXES)),
  known: TOOL_NAMES.filter((n) => toolsMode.PRIVATE_CORE_TOOLS.has(n) || BOXES.some((b) => b.tools.some((t) => t.function.name === n))),
};
const PROVIDER = 'http://qa-default.invalid';
const DIARY_BASE = 'http://qa-diary.invalid';
// The files retrieval was given, as indices into the stored record's files: the given ones are copies (with accessors),
// so they are matched by their JSON in order (equal files share every verdict, so the earliest equal one is the one).
function storedAt(storedFiles, given) {
  const list = (Array.isArray(storedFiles) ? storedFiles : []).map((f) => JSON.stringify(f));
  let at = 0;
  return (Array.isArray(given) ? given : []).map((f) => {
    const text = JSON.stringify(f);
    while (at < list.length && list[at] !== text) at += 1;
    if (at >= list.length) throw new Error('retrieval was given a file the project does not hold');
    at += 1;
    return at - 1;
  });
}
function handlerFor(dir, spec, log) {
  const fetch = async (url, init) => {
    if (!String(url).startsWith(PROVIDER)) throw new Error('QA: no network');
    log.requests.push(JSON.parse(init.body));
    return { ok: true, body: (async function* () { yield Buffer.from(`data: ${JSON.stringify({ choices: [{ delta: { content: 'QA ok' } }] })}\n\n`); })() };
  };
  // getProject: the stored record with source-text.cjs's accessors, its texts where they live (projects.cjs loads it so).
  const stored = (id) => {
    if (id !== spec.project.id) return null;
    const p = structuredClone(spec.project);
    for (const f of Array.isArray(p.files) ? p.files : []) sourceText.attachFile(f, dir, p.id);
    return p;
  };
  return createChatHandler({
    modelManager: { enabled: true, health: async () => ({ ok: true, body: { all_models_loaded: [{ model_name: 'qa-model', loaded: true, recipe_options: { ctx_size: 1048576 } }] } }) },
    reasoningEffort: req('reasoning-effort.cjs'), authService: { audit() {}, diaryEnabled: () => true }, crypto, path, fs, fetch,
    HISTORY_CAP: 1000, DEFAULT_PROVIDER_ID: 'default', createToolExchange: () => async () => 'unused',
    currentWorkspace: () => ({ userId: 'qa-user', dir, assetDir: () => '/qa-only' }),
    getProject: stored, skillsIndexFor: () => [],
    getProvider: () => ({ id: 'default', baseUrl: PROVIDER }), providerHeaders: () => ({}),
    autoRoles: () => null, visionDescriptions: new Map(), visionProbe: async () => ({ supported: false, reason: 'qa' }),
    chatSkillRouter: { select: async () => ({ loaded: [] }) }, oauthServerIds: () => new Set(), accountReady: () => true, mcpOAuth: { connected: () => false },
    chatToolRouter: { select: async (ids) => ({ ids, routed: false }) }, DEFAULT_TOOLBOXES: [], CONNECTOR_BOXES: new Set(), connectedBoxes: () => [],
    toolPolicy: { mode: () => 'allow' }, requestScope: { getStore: () => ({}) }, resolveTools: () => ({ tools: [], dropped: [] }), isWriteTool: () => false,
    rag: {
      filesContext: async (projectId, files, query, userId, onSources, options) => {
        log.calls.push({ seam: 'filesContext', projectId, readable: storedAt(spec.project.files, files), query, user: userId, onSources: typeof onSources,
          options: { attached: options?.attached, images: options?.images, onImages: typeof options?.onImages } });
        const a = spec.answer || {};
        if (a.throws) throw new Error('QA retrieval failed');
        if (!Array.isArray(files) || files.length === 0) return null;
        if (options?.images === true && typeof options.onImages === 'function') { try { options.onImages({ hits: [], indexed: [] }); } catch { /* as rag.cjs */ } }
        if (a.report !== false && typeof onSources === 'function') {
          try { onSources('placed' in a ? a.placed : [], 'injected' in a ? a.injected : []); } catch { /* as rag.cjs: a reporting hook never breaks the prompt */ }
        }
        return a.block === undefined ? null : a.block;
      },
    },
    prefill: { recordSample() {} }, reduceToolResult: () => ({ text: 'reduced' }), diaryExtras,
    DIARY_BASE, TOOL_RESULT_CAP: 8000, json: (res, status, body) => { throw new Error(`QA turn answered ${status}: ${JSON.stringify(body)}`); },
    saveChats() {}, endpointApproved: () => true, diaryHeaders: () => ({}), lastLoadedModel: () => 'qa-model', classifyFastOrSmart: async () => 'fast',
    servedCatalogue: async () => [], modelsInstalled: async () => [], missingRoles: () => [], staleRolesError: () => null, allToolboxes: () => BOXES,
    chatWideApproved: () => false, recordUsage() {}, recordToolUse() {}, executeToolCall: async () => 'unused', awaitApproval: async () => 'deny',
    codeTasksFor: () => [],
    sourceJobs: {
      settle: async (workspace, projectId, opts) => {
        log.calls.push({ seam: 'settle', user: workspace?.userId, projectId, timeoutMs: opts?.timeoutMs, signal: !!opts?.signal });
        return { notReady: spec.notReady || [], aborted: spec.aborted === true };
      },
    },
    skillHistory: { scrub: ({ messages }) => ({ messages, removed: [] }), record() {} },
    multimodalEnabled: () => spec.multimodal, chatFramingEnabled: () => false,
    nativeHandoff: { requested: () => true, capabilities: () => new Set(['skills']), supervising: () => false },
  });
}
const turns = [];
async function addTurn(name, rawSpec) {
  const spec = viaJson({ message: 'QA question', multimodal: false, ...rawSpec });
  const dir = fs.mkdtempSync(path.join(tmp, 'qa-'));
  const texts = writeTexts(dir, spec.project.id, spec.texts);
  process.env.NOEVIA_CONTRACT_FIXED_NOW = '2026-11-13T12:00:00Z';
  process.env.TZ = 'UTC';
  const log = { requests: [], calls: [] };
  const raw = [];
  const res = new EventEmitter();
  res.writeHead = () => {}; res.setHeader = () => {};
  res.write = (line) => { raw.push(String(line)); return true; };
  res.end = () => { res.writableEnded = true; res.emit('finish'); };
  const { handleChat } = handlerFor(dir, spec, log);
  const body = { message: spec.message, ...(spec.free ? {} : { projectId: spec.project.id }), ...(spec.history === undefined ? {} : { history: spec.history }), ...(spec.body || {}) };
  const from = logged.length;
  let thrown = null;
  try { await handleChat({ headers: {} }, res, body, { user: { id: 'qa-user' } }); } catch (e) { thrown = e.constructor.name; process.stderr.write(`QA ${name} threw: ${e.stack}\n`); }
  await new Promise((resolve) => setImmediate(resolve));
  const events = raw.join('').split('\n\n').filter((x) => x.startsWith('data: ')).map((x) => JSON.parse(x.slice(6)));
  const prepared = events.find((e) => e.type === 'prepared') || null;
  const meta = events.find((e) => e.type === 'meta') || null;
  const sourcesEvent = events.find((e) => e.type === 'sources') || null;
  const refused = logged.slice(from).map((l) => /^\[chat\] native executor not used for this turn: (.+)$/.exec(l)?.[1]).find(Boolean) || null;
  const outcome = thrown ? { throws: thrown }
    : prepared ? { frame: { messages: prepared.messages, sources: prepared.turn.sources, coverageWarning: prepared.skills ? prepared.skills.coverageWarning : null } }
    : spec.aborted ? { ended: events.length === 0 && log.requests.length === 0 }
    : { refused, messages: log.requests.length === 1 ? log.requests[0].messages : null };
  if (!thrown && !prepared && !spec.aborted && !spec.refusedOk) throw new Error(`${name}: no frame (refused: ${refused}); events: ${raw.join('').slice(0, 600)}`);
  if (outcome.messages === null && !spec.diarySpace) throw new Error(`${name}: ${log.requests.length} model requests`);
  turns.push({ name, ...(spec.diarySpace ? { diarySpace: true } : {}), body, project: spec.project, texts, multimodal: spec.multimodal ?? null, answer: spec.answer || {},
    settle: { notReady: spec.notReady || [], aborted: spec.aborted === true }, calls: log.calls,
    notReady: meta ? meta.sourcesNotReady ?? null : null, sourcesEvent: sourcesEvent ? sourcesEvent.sources : null, ...outcome });
}
const U = (content) => ({ role: 'user', content }), A = (content) => ({ role: 'assistant', content });
const P = (extra = {}) => ({ id: 'qa-project-1', name: 'QA Project', ...extra });
const SHADOW = 'cowork-chat-context-qa-chat-1';
const queued = (name, extra = {}) => ({ name, content: '', document: { state: 'queued' }, ...extra });
const ref = (text) => sourceText.refOf(text);
const EXT_TEXT = `QA external text. ${'e'.repeat(5000)}`;

(async () => {
  await addTurn('no_project_files', { project: P() });
  await addTurn('empty_files', { project: P({ files: [] }), answer: { block: 'QA never asked' } });
  await addTurn('all_unreadable', { project: P({ files: [{ name: 'QA stored.txt', attachment: { state: 'stored', group: 'Text' } },
    { name: 'QA failed.pdf', document: { state: 'failed' }, content: '' }, queued('QA scan.pdf')] }), answer: { block: 'QA unused', placed: [{ file: 'QA stored.txt', body: 'QA' }] } });
  await addTurn('only_skills', { project: P({ files: [SKILL('QA/SKILL.md')] }), answer: { block: 'QA unused' } });
  await addTurn('waiting_documents', { project: P({ files: [queued('QA scan 1.pdf'), queued('QA scan 2.pdf'), plain('QA ok.txt')] }),
    answer: { block: 'QA block', placed: [], injected: [{ file: 'QA ok.txt', body: 'QA text', kind: 'file' }] } });
  await addTurn('not_ready_failed', { project: P({ files: [plain('QA ok.txt')] }), notReady: [{ name: 'QA late.pdf', reason: 'failed' }],
    answer: { block: 'QA block', placed: [], injected: [] } });
  await addTurn('not_ready_mixed', { project: P({ files: [plain('QA ok.txt')] }), notReady: [{ name: 'QA a.pdf', reason: 'failed' }, { name: 'QA "b"\n.pdf', reason: 'timeout' }] });
  await addTurn('not_ready_without_files', { project: P(), notReady: [{ name: 'QA late.txt', reason: 'timeout' }] });
  await addTurn('settle_aborted', { project: P({ files: [plain('QA ok.txt')] }), aborted: true, refusedOk: true });
  await addTurn('placed_and_injected', { project: P({ files: [plain('QA notes.txt', { attachment: { id: 'qa-att-1', state: 'ready', group: 'Text' } }),
    plain('QA big.txt', { content: BIG }), plain('QA other big.txt', { content: BIG + BIG }), plain('QA small.txt', { content: 'QA small 42 text' })] }),
  answer: { block: 'QA excerpt block', placed: [{ file: 'QA big.txt', body: '[Page 3]\nQA big source text.', score: 0.87654, kind: 'excerpt' }, { file: 'QA notes.txt', body: 'QA text', kind: 'file' }],
    injected: [{ file: 'QA big.txt', body: '[Page 3]\nQA big source text.', score: 0.87654, kind: 'excerpt' }, { file: 'QA notes.txt', body: 'QA text', kind: 'file' },
      { file: 'QA small.txt', body: 'QA small 42 text', kind: 'file' }, { file: 'QA ghost.txt', body: 'QA not a project file', kind: 'file' }] } });
  await addTurn('injected_not_list_uses_placed', { project: P({ files: [plain('QA big.txt', { content: BIG })] }),
    answer: { block: 'QA block', placed: [{ file: 'QA big.txt', body: 'x'.repeat(100), kind: 'excerpt' }], injected: null } });
  await addTurn('placed_not_list', { project: P({ files: [plain('QA big.txt', { content: BIG })] }), answer: { block: 'QA block', placed: { file: 'QA big.txt' }, injected: 'QA' } });
  await addTurn('no_report', { project: P({ files: [plain('QA big.txt', { content: BIG })] }), answer: { block: 'QA block', report: false } });
  await addTurn('block_null_with_report', { project: P({ files: [plain('QA big.txt', { content: BIG })] }), answer: { block: null, placed: [], injected: [] } });
  await addTurn('block_empty', { project: P({ files: [plain('QA a.txt')] }), answer: { block: '', placed: [{ file: 'QA a.txt', body: 'QA text', kind: 'file' }] } });
  await addTurn('coverage_each_kind', { project: P({ files: [plain('QA none.txt', { content: BIG }), plain('QA some.txt', { content: BIG }), plain('QA head.txt', { content: BIG }),
    plain('QA whole.txt', { content: BIG }), plain('QA small.txt', { content: 's'.repeat(2400) }), plain('QA astral.txt', { content: '😀'.repeat(1300) })] }),
  answer: { block: 'QA block', placed: [{ file: 'QA some.txt', body: 'x'.repeat(1234), kind: 'excerpt' }],
    injected: [{ file: 'QA some.txt', body: 'x'.repeat(1234), kind: 'excerpt' }, { file: 'QA head.txt', body: BIG.slice(0, 1500), kind: 'file' },
      { file: 'QA whole.txt', body: BIG, kind: 'file' }, { file: 'QA small.txt', body: 'QA', kind: 'file' }, { file: 'QA astral.txt', body: '😀', kind: 'excerpt' }] } });
  await addTurn('many_files', { project: P({ files: Array.from({ length: 30 }, (_, i) => plain(`QA file ${i}.txt`, { content: i % 3 ? `QA text ${i}` : BIG, attachment: { id: `qa-att-${i}` } })) }),
    answer: { block: 'QA many', placed: Array.from({ length: 20 }, (_, i) => ({ file: `QA file ${i}.txt`, body: `QA body ${i} INV-20${i}`, score: i / 7, kind: i % 2 ? 'excerpt' : 'file' })),
      injected: Array.from({ length: 30 }, (_, i) => ({ file: `QA file ${i}.txt`, body: `QA body ${i} INV-20${i}`, kind: 'file' })) } });
  await addTurn('names_astral_and_lone', { project: P({ files: [plain('QA 😀 astral.md', { content: BIG }), plain('QA \ud83d lone.txt'), plain('QA 日本語/notes.md')] }),
    answer: { block: 'QA block 😀', placed: [{ file: 'QA 😀 astral.md', body: '😀'.repeat(150), kind: 'excerpt' }],
      injected: [{ file: 'QA 😀 astral.md', body: '😀'.repeat(150), kind: 'excerpt' }, { file: 'QA \ud83d lone.txt', body: 'QA text', kind: 'file' }, { file: 'QA 日本語/notes.md', body: 'QA text', kind: 'file' }] } });
  await addTurn('non_text_kinds', { project: P({ files: [plain('QA a.txt'), { name: 'QA image.png', attachment: { state: 'ready', group: 'Images', id: 'qa-img' }, content: 'QA caption' }] }),
    answer: { block: 'QA block', placed: [{ file: 'QA a.txt', body: 'QA', kind: 'Excerpt' }, { file: 'QA image.png', body: 'QA caption', kind: 'image' }, null, 5],
      injected: [{ file: 'QA a.txt', body: 'QA', kind: 'file' }, { file: 'QA image.png', body: 'QA caption', kind: 'file', score: 0.5 }, { body: 'QA nameless', kind: 'excerpt' }] } });
  await addTurn('skill_file_not_given', { project: P({ files: [SKILL('QA/SKILL.md'), plain('QA a.txt')] }),
    answer: { block: 'QA block', placed: [{ file: 'QA/SKILL.md', body: 'QA', kind: 'file' }, { file: 'QA a.txt', body: 'QA', kind: 'file' }],
      injected: [{ file: 'QA/SKILL.md', body: 'QA', kind: 'file' }, { file: 'QA a.txt', body: 'QA', kind: 'file' }] } });
  await addTurn('report_hook_throws_in_sources', { project: P({ files: [plain('QA big.txt', { content: BIG })] }),
    answer: { block: 'QA block', placed: [{ file: 'QA big.txt', body: { toString: 1 }, kind: 'excerpt' }], injected: [{ file: 'QA other.txt', body: 'QA', kind: 'file' }] } });
  await addTurn('report_hook_throws_in_coverage', { project: P({ files: [plain('QA big.txt', { content: BIG })] }),
    answer: { block: 'QA block', placed: [{ file: 'QA big.txt', body: 'QA', kind: 'excerpt' }], injected: [{ file: 'QA big.txt', body: { toString: 1 }, kind: 'excerpt' }] } });
  await addTurn('frame_sources_throw', { project: P({ files: [plain('QA a.txt')] }),
    answer: { block: 'QA block', placed: [], injected: [{ file: 'QA ghost.txt', body: { toString: 1 }, kind: 'file' }] } });
  await addTurn('injected_name_not_text', { project: P({ files: [plain('QA a.txt')] }), refusedOk: true,
    answer: { block: 'QA block', placed: [], injected: [{ file: 5, body: 'QA', kind: 'file' }] } });
  await addTurn('retrieval_throws', { project: P({ files: [plain('QA a.txt')] }), answer: { throws: true } });
  await addTurn('null_file_throws', { project: P({ files: [plain('QA a.txt'), null] }), answer: { block: 'QA block' } });
  // Sources whose text is in its own file (#1806): the length comes from `chars`, the text through the accessor.
  await addTurn('external_texts', { texts: [EXT_TEXT, 'QA short external'], project: P({ files: [
    { name: 'QA ext big.txt', textRef: ref(EXT_TEXT), chars: EXT_TEXT.length },
    { name: 'QA ext missing.txt', textRef: 'd'.repeat(64), chars: 7000 },
    { name: 'QA ext chars text.txt', textRef: ref(EXT_TEXT), chars: '5018' },
    { name: 'QA ext short.txt', textRef: ref('QA short external'), chars: 17 },
    { name: 'QA ext queued.pdf', textRef: 'e'.repeat(64), chars: 9000, document: { state: 'queued' } },
    // A skill by its saved flag alone (its text is not read): not given to retrieval.
    { name: 'QA ext skill.md', textRef: 'f'.repeat(64), chars: 10, skillHeader: true }] }),
  answer: { block: 'QA ext block', placed: [{ file: 'QA ext big.txt', body: 'x'.repeat(300), kind: 'excerpt' }],
    injected: [{ file: 'QA ext big.txt', body: 'x'.repeat(300), kind: 'excerpt' }, { file: 'QA ext short.txt', body: 'QA short external', kind: 'file' }] } });
  await addTurn('external_textref_kinds', { texts: [EXT_TEXT], project: P({ files: [
    { name: 'QA list ref.txt', textRef: [ref(EXT_TEXT)], chars: EXT_TEXT.length },
    { name: 'QA upper ref.txt', textRef: ref(EXT_TEXT).toUpperCase(), chars: 9000 },
    { name: 'QA inline empty.txt', textRef: ref(EXT_TEXT), content: '', chars: 9000 },
    { name: 'QA negative.txt', textRef: ref(EXT_TEXT), chars: -5 },
    { name: 'QA fraction.txt', textRef: ref(EXT_TEXT), chars: 100.5 },
    { name: 'QA unsafe missing.txt', textRef: 'c'.repeat(64), chars: 9007199254740992 },
    { name: 'QA list skill.md', textRef: ['f'.repeat(64)], chars: 10, skillHeader: true }] }),
  answer: { block: 'QA ext block', placed: [{ file: 'QA list ref.txt', body: 'x'.repeat(400), kind: 'excerpt' }],
    injected: [{ file: 'QA list ref.txt', body: 'x'.repeat(400), kind: 'excerpt' }, { file: 'QA negative.txt', body: 'QA', kind: 'file' }] } });
  // A free chat's attachments (#1441): the shadow project, asked as `attached`; its block rides on the user's message.
  await addTurn('chat_attachments', { free: true, body: { spaceId: 'free', chatId: 'qa-chat-1' }, project: { id: SHADOW, name: 'QA hidden', files: [plain('QA attached.txt')] },
    answer: { block: 'QA attached excerpt', placed: [{ file: 'QA attached.txt', body: 'QA text', kind: 'file' }], injected: [{ file: 'QA attached.txt', body: 'QA text', kind: 'file' }] },
    notReady: [{ name: 'QA late.txt', reason: 'timeout' }], history: [U('QA earlier'), A('QA reply')] });
  await addTurn('chat_attachments_by_project_id', { project: { id: SHADOW, files: [plain('QA attached.txt')] }, answer: { block: 'QA attached excerpt' } });
  await addTurn('free_chat_other_context', { free: true, body: { spaceId: 'free', chatId: 'qa-chat-2' }, project: { id: SHADOW, files: [plain('QA attached.txt')] } });
  await addTurn('free_chat_id_not_safe', { free: true, body: { spaceId: 'free', chatId: 'qa chat 1' }, project: { id: 'cowork-chat-context-qa chat 1', files: [plain('QA a.txt')] } });
  await addTurn('free_chat_no_space', { free: true, body: { chatId: 'qa-chat-1' }, project: { id: SHADOW, files: [plain('QA a.txt')] } });
  // Multimodal retrieval (#1726): asked for images only when on and not a Diary turn; never on compactOnly.
  await addTurn('multimodal_on', { multimodal: true, project: P({ files: [plain('QA a.txt')] }), answer: { block: 'QA block', placed: [], injected: [] } });
  await addTurn('multimodal_on_attachments', { multimodal: true, free: true, body: { spaceId: 'free', chatId: 'qa-chat-1' }, project: { id: SHADOW, files: [plain('QA a.txt')] },
    answer: { block: 'QA block' } });
  await addTurn('multimodal_not_true', { multimodal: 'true', project: P({ files: [plain('QA a.txt')] }), answer: { block: 'QA block' } });
  await addTurn('multimodal_diary_context', { multimodal: true, free: true, body: { spaceId: 'free', chatId: 'diary-qa-1' }, project: { id: 'cowork-chat-context-diary-qa-1', files: [plain('QA a.txt')] },
    answer: { block: 'QA block' } });
  await addTurn('multimodal_diary_context_upper', { multimodal: true, project: { id: 'COWORK-CHAT-CONTEXT-DIARY_qa', files: [plain('QA a.txt')] }, answer: { block: 'QA block' } });
  await addTurn('multimodal_diary_extras_id', { multimodal: true, project: { id: 'cowork-diary-extras', files: [plain('QA a.txt')] }, answer: { block: 'QA block' }, refusedOk: true });
  // Diary spaces: the extras project (a Diary turn, which the executor refuses), another `diary…` space, the Diary itself.
  await addTurn('diary_extras_space', { multimodal: true, free: true, refusedOk: true, body: { spaceId: 'diary-extras', extrasEnabled: true, sessionId: 'qa-session' },
    project: { id: 'cowork-diary-extras', name: 'QA extras', files: [plain('QA diary attachment.txt')] }, notReady: [{ name: 'QA late.txt', reason: 'failed' }],
    answer: { block: 'QA diary block', placed: [{ file: 'QA diary attachment.txt', body: 'QA', kind: 'file' }], injected: [] } });
  await addTurn('diary_other_space', { multimodal: true, refusedOk: true, body: { spaceId: 'diary-qa' }, project: P({ files: [plain('QA a.txt')] }), answer: { block: 'QA block' } });
  // The Diary itself: no project, no wait, no retrieval (its own companion stream answers; the fetch stub refuses it).
  await addTurn('diary_space', { multimodal: true, refusedOk: true, diarySpace: true, free: true, body: { spaceId: 'diary' }, project: P({ files: [plain('QA a.txt')] }), answer: { block: 'QA block' } });
  // With a projectId as well (no client sends one): neither seam is asked; Node then throws at the grant holder
  // (chat.cjs `expectedGrantHolder` reads `project.id` of the project it did not load), outside this slice.
  await addTurn('diary_space_project_id', { multimodal: true, refusedOk: true, diarySpace: true, body: { spaceId: 'diary' }, project: P({ files: [plain('QA a.txt')] }), answer: { block: 'QA block' } });
  // compactOnly: no wait, no images, the block in the system message even for attachments, no `sources` event.
  await addTurn('compact_project', { multimodal: true, project: P({ files: [plain('QA a.txt'), plain('QA big.txt', { content: BIG })] }), message: '', history: [U('q1'), A('a1')],
    body: { compactOnly: true, chatId: 'qa-compact-chat' }, notReady: [{ name: 'QA ignored', reason: 'failed' }],
    answer: { block: 'QA block', placed: [{ file: 'QA a.txt', body: 'QA text', kind: 'file' }], injected: [{ file: 'QA a.txt', body: 'QA text', kind: 'file' }, { file: 'QA big.txt', body: 'b', kind: 'file' }] } });
  await addTurn('compact_attachments', { project: { id: SHADOW, files: [plain('QA attached.txt')] }, message: '', history: [U('q1'), A('a1')],
    body: { compactOnly: true, chatId: 'qa-chat-1' }, answer: { block: 'QA attached excerpt', placed: [{ file: 'QA attached.txt', body: 'QA', kind: 'file' }] } });
  await addTurn('question_text', { project: P({ files: [plain('QA a.txt')] }), message: 'QA what does "QA a.txt" say?\n😀 \ud83d', answer: { block: 'QA block' } });
  // Random turns.
  for (let n = 0; n < 120; n += 1) {
    const shadow = rand() < 0.15;
    const project = { id: shadow ? `${SHADOW}-${n}` : `qa-project-r${n}` };
    if (rand() < 0.8) project.name = pick(['QA Project', 'QA "quoted"', '😀 QA']);
    const files = Array.from({ length: rand() < 0.15 ? 0 : int(1, 9) }, (_, i) => {
      if (rand() < 0.08) return SKILL(`QA s${i}/SKILL.md`);
      const f = fileOf(i, [...NAMES, `QA file ${i}.txt`], true);
      if (!f || typeof f !== 'object' || Array.isArray(f)) return plain(`QA plain ${i}.txt`);
      if (typeof f.name !== 'string') f.name = `QA file ${i}.txt`;
      if (f.content && typeof f.content === 'object' && !Array.isArray(f.content)) f.content = 'QA';
      if (f.attachment && typeof f.attachment === 'object' && f.attachment.id && typeof f.attachment.id === 'object' && !Array.isArray(f.attachment.id)) f.attachment.id = 'qa-id';
      return f;
    });
    if (files.length || rand() < 0.5) project.files = files;
    const named = files.map((f) => f.name);
    const pool = named.length ? named : NAMES;
    const injected = Array.from({ length: int(0, 8) }, () => {
      const e = entryOf(pool);
      if (e && typeof e === 'object') {
        if (e.body && typeof e.body === 'object' && !Array.isArray(e.body)) e.body = 'QA';
        if (e.kind === 'file' && typeof e.file !== 'string') e.file = pick(pool);
      }
      return e;
    });
    const placed = injected.filter((e) => e && rand() < 0.4);
    const answer = rand() < 0.1 ? { block: pick([null, '']) } : {
      block: rand() < 0.85 ? textOf(int(1, 12)) : pick([null, '']),
      placed: rand() < 0.05 ? pick([null, 'QA']) : placed,
      ...(rand() < 0.85 ? { injected: rand() < 0.05 ? null : injected } : {}),
    };
    const free = shadow && rand() < 0.5;
    await addTurn(`random_${n}`, {
      project, answer, multimodal: rand() < 0.1,
      notReady: rand() < 0.2 ? Array.from({ length: int(1, 3) }, () => ({ name: pick(pool), reason: pick(['failed', 'timeout']) })) : [],
      message: textOf(int(1, 8)) || 'QA',
      history: Array.from({ length: int(0, 4) }, (_, i) => (i % 2 ? A(textOf(int(1, 4)) || 'QA') : U(textOf(int(1, 4)) || 'QA'))),
      ...(free ? { free: true, body: { spaceId: 'free', chatId: project.id.slice('cowork-chat-context-'.length) } } : {}),
    });
  }
  delete process.env.NOEVIA_CONTRACT_FIXED_NOW;

  // ── Differences ─────────────────────────────────────────────────────────────────────────
  // The port keeps a turn whose request asks for images in Node (what retrieval finds of them is a later slice);
  // Node runs it, handing it to the executor when it found none.
  const differences = turns.filter((t) => t.calls.some((c) => c.seam === 'filesContext' && c.options.images === true))
    .map((t) => ({ kind: 'retrieved_images', name: t.name, nodeFrame: !!t.frame,
      about: 'Multimodal retrieval was asked for images: the port stops (Stopped::Images) and the turn stays in Node.' }));

  // ── Output ──────────────────────────────────────────────────────────────────────────────
  const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
  const text = `{"version":1,"generator":"crates/chat-prepare/tests/record_retrieval.cjs",\n"node":${JSON.stringify(process.version)},\n`
    + `"sources":${JSON.stringify(sources)},\n"blocks":${JSON.stringify(blocks)},\n"turnTools":${JSON.stringify(turnTools)},\n`
    + `"build":[\n${lines(buildCases)}\n],\n"partial":[\n${lines(partialCases)}\n],\n"warning":[\n${lines(warningCases)}\n],\n`
    + `"injected":[\n${lines(injectedCases)}\n],\n"readable":[\n${lines(readableCases)}\n],\n`
    + `"diaryProject":[\n${lines(diaryProjectCases)}\n],\n"chatProject":[\n${lines(chatProjectCases)}\n],\n`
    + `"turns":[\n${lines(turns)}\n],\n"differences":[\n${lines(differences)}\n]}\n`;
  const check = process.argv.indexOf('--check');
  const counts = `${buildCases.length} build, ${partialCases.length} partial, ${warningCases.length} warning, ${injectedCases.length} injected, `
    + `${readableCases.length} readable, ${diaryProjectCases.length} diaryProject, ${chatProjectCases.length} chatProject, `
    + `${turns.length} turns (${turns.filter((t) => t.throws).length} throw, ${turns.filter((t) => t.frame).length} frames), ${differences.length} differences`;
  if (check > 0) {
    const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
    if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
    process.stderr.write(`ok: ${counts} recorded as committed\n`, () => process.exit(0));
  } else {
    process.stderr.write(`recorded: ${counts}\n`);
    // The partial-save timers of chat-id turns would keep the process alive: leave once stdout is written.
    process.stdout.write(text, () => process.exit(0));
  }
})().catch((e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
