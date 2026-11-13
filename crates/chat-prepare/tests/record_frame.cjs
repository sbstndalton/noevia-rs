#!/usr/bin/env node
'use strict';
// Recorded chat frame steering for crates/chat-prepare (slice P4a): the contract src/frame.rs is replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-prepare/tests/record_frame.cjs > crates/chat-prepare/tests/fixtures/node-frame.v1.json
//   ... node crates/chat-prepare/tests/record_frame.cjs --check crates/chat-prepare/tests/fixtures/node-frame.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// The checkout needs server/wasm/dav-parse.wasm (frameUntrusted is the Rust port in it); nothing from node_modules.
// Every expectation comes from noevia-core's own code; the inputs are synthetic and QA-named (a seeded PRNG plus hand-made
// edges), passed through JSON first so Node reads exactly what is recorded:
//   normalize  chat-framing.cjs normalizeFrame(frame)
//   stored     chat-frame-steering.cjs storedFrame(input), then frameBlock and gateBias of what it returned
//   blocks     frameBlock(frame) and gateBias(frame) of a frame in storedFrame's shape given directly (tags not cleaned)
//   sweep      every code point c (lone surrogates included) as the only tag of a confirmed question frame, alone and
//              after 'a': the tag frameBlock shows, as runs kept unchanged, runs removed, and the changed ones listed
// Each source file's sha256 is recorded, so a changed Node file needs a fresh recording. Node's console output goes to
// stderr; stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => process.stderr.write(`${a.map(String).join(' ')}\n`);
// server/chat.cjs is not loaded: its call site (~740-770, the rules src/frame.rs chat_frame ports) is pinned by its sha.
const SOURCE_FILES = ['server/chat-frame-steering.cjs', 'server/chat-framing.cjs', 'server/tool-router.cjs', 'server/project-modes.cjs',
  'server/prompt-framing.cjs', 'server/dav-parse-wasm.cjs', 'server/dav-parse.lock', 'server/wasm/dav-parse.wasm', 'server/chat.cjs'];
const sources = Object.fromEntries(SOURCE_FILES.map((f) => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex')]));
const req = (f) => require(path.join(root, 'server', f));
const steering = req('chat-frame-steering.cjs');
const { normalizeFrame, KINDS } = req('chat-framing.cjs');

// ── Deterministic inputs ────────────────────────────────────────────────────────────────────
let seed = 0x2026_0739;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const pick = (list) => list[Math.floor(rand() * list.length)];
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

// Tag pieces: plain, duplicates, '#', whitespace, NFKC-changing forms (full-width, ligatures, compatibility digits and
// letters, half-width kana that compose), punctuation and emoji (removed), astral letters (U+1D400 folds to 'A' under
// NFKC; U+10400 and U+20000 do not), combining marks (removed unless NFKC composes them), lone surrogates.
const PIECES = ['qa', 'QA', 'qa-notes', 'qa_notes', 'notes', '#qa', '##qa', '#', ' ', '  ', '\t', '\n', '　', ' ', '﻿',
  '​', '᠎', '\u0085', 'Ｑａ', 'ｑａ１２', 'ﬁ', 'ﬃle', 'ﬆ', '①', '²', '½', '٣', '෧', 'Ⅻ', 'ⅶ', 'ǅ', 'ʰ', 'ſ', 'İ', 'ß', 'Å', 'Ω',
  'ｶ', 'ｶﾞ', 'ﾊﾟ', '㌀', '㍻', '㈱', '℡', '™', 'ℌ', '.', ',', '!', '?', ':', '：', '/', '\\', '"', '<', '>', '&', '‐', '–', '—', '_',
  '-', '－', '＿', '😀', '👍🏽', '🇩🇪', '❤️', '#️⃣', '𝐀', '𝐀𝐁', '𝟘', '𐐀', '𠀀', '𞤀', 'é', 'ä', '́', 'q⃝a', 'क्ष',
  'ก่', 'ั', '日本語', 'Ünïcode', 'çà', '­', '‍', '⁦', '\ud83d', '\ude00', '\ud801', 'x'.repeat(31) + '𐐀',
  'y'.repeat(31) + '😀', 'z'.repeat(40), '1234567890', '٠١٢', 'Ⅰ'];
const tagOf = (n) => Array.from({ length: n }, () => pick(PIECES)).join('');
const tagsOf = (n) => Array.from({ length: n }, () => (rand() < 0.08 ? pick([5, null, true, {}, ['qa'], '', 0]) : tagOf(int(1, 4))));

// ── normalizeFrame ──────────────────────────────────────────────────────────────────────────
const normalize = [];
const addNormalize = (name, raw) => {
  const frame = viaJson(raw);
  normalize.push({ name, frame, out: normalizeFrame(frame) });
};

// ── storedFrame, frameBlock, gateBias ───────────────────────────────────────────────────────
const stored = [];
const addStored = (name, raw) => {
  const input = viaJson(raw);
  const frame = steering.storedFrame(input);
  stored.push({ name, input, frame, block: steering.frameBlock(frame), bias: steering.gateBias(frame) });
};
const blocks = [];
const addBlock = (name, raw) => {
  const frame = viaJson(raw);
  blocks.push({ name, frame, block: steering.frameBlock(frame), bias: steering.gateBias(frame) });
};

const PID = 'qa-project-1';
const F = (kind, extra = {}) => ({ projectId: null, kind, tags: [], links: [], confirmed: true, source: 'user', ...extra });
const C = (id, frame, extra = {}) => ({ id, title: `QA chat ${id}`, ...(frame === undefined ? {} : { frame }), ...extra });
const P = (chats, extra = {}) => ({ id: PID, name: 'QA Project', chats, ...extra });
const free = (frame) => ({ enabled: true, chatId: 'qa-chat-1', projectId: null, project: null, freeChats: [C('qa-chat-1', frame)] });
const inProj = (frame, extra = {}) => ({ enabled: true, chatId: 'qa-chat-1', projectId: PID, project: P([C('qa-chat-1', frame)]), freeChats: [], ...extra });

// Kinds, confirmed, source, links, frame shapes.
for (const kind of [...KINDS, 'unknown', 'Search', 'QUESTION', '', 5, null]) {
  addStored(`free_kind_${kind}`, free(F(kind)));
  addStored(`project_kind_${kind}`, inProj(F(kind, { projectId: PID })));
  addStored(`project_kind_${kind}_tags`, inProj(F(kind, { projectId: PID, tags: ['qa', 'notes'] })));
}
for (const confirmed of [true, false, 'true', 1, null, undefined]) addStored(`confirmed_${String(confirmed)}`, free(F('search', { confirmed })));
for (const source of ['user', 'suggested', 'other', '', 5, undefined]) addStored(`source_${String(source)}`, free(F('idea', { source })));
for (const links of [['qa-chat-2', 'qa-chat-2', '', 5, 'x'.repeat(129), 'x'.repeat(128)], 'qa-chat-2', { 0: 'qa' }, null, 5]) {
  addStored(`links_${JSON.stringify(links).slice(0, 20)}`, free(F('code', { links })));
}
for (const frame of ['search', ['search'], null, true, 5, {}, { kind: 'search' }, { kind: 'search', confirmed: true }]) {
  addStored(`frame_${JSON.stringify(frame)}`, free(frame));
}
addStored('no_frame_key', free(undefined));
// Flag, chat id.
addStored('enabled_false', { ...inProj(F('search', { projectId: PID })), enabled: false });
addStored('enabled_missing', { ...free(F('search')), enabled: undefined });
for (const chatId of [undefined, '', 5, null, ['qa-chat-1'], 'qa-missing', 'QA-CHAT-1', 'qa-chat-1 ']) {
  addStored(`chat_id_${JSON.stringify(chatId)}`, { ...inProj(F('search', { projectId: PID })), chatId });
}
// Which list: project only, free only, both, project entry without a frame or unconfirmed.
addStored('project_list_only', inProj(F('action', { projectId: PID })));
addStored('free_list_only_with_project', { ...inProj(F('action', { projectId: PID })), project: P([C('qa-chat-9', F('code'))]),
  freeChats: [C('qa-chat-1', F('action', { projectId: PID }))] });
addStored('both_lists_project_wins', { ...inProj(F('code', { projectId: PID })), freeChats: [C('qa-chat-1', F('search'))] });
addStored('both_lists_project_entry_frameless', { ...inProj(undefined), freeChats: [C('qa-chat-1', F('search'))] });
addStored('both_lists_project_entry_unconfirmed', { ...inProj(F('code', { confirmed: false })), freeChats: [C('qa-chat-1', F('search'))] });
addStored('both_lists_project_entry_not_a_frame', { ...inProj(F('nope')), freeChats: [C('qa-chat-1', F('search'))] });
addStored('no_project_chat_in_free', { ...free(F('idea', { projectId: PID })), projectId: PID });
addStored('duplicate_ids_first_wins', { ...free(F('idea')), freeChats: [C('qa-chat-1', F('idea')), C('qa-chat-1', F('search'))] });
addStored('duplicate_ids_first_frameless', { ...free(F('idea')), freeChats: [C('qa-chat-1'), C('qa-chat-1', F('search'))] });
// projectId and the frame's projectId.
for (const projectId of [PID, 'qa-project-2', null, undefined, '', 5]) {
  for (const frameProject of [PID, 'qa-project-2', null, '', 5]) {
    addStored(`project_id_${JSON.stringify(projectId)}_frame_${JSON.stringify(frameProject)}`,
      { ...inProj(F('search', { projectId: frameProject, tags: ['qa'] })), projectId });
    addStored(`free_project_id_${JSON.stringify(projectId)}_frame_${JSON.stringify(frameProject)}`,
      { ...free(F('search', { projectId: frameProject })), projectId });
  }
}
addStored('project_record_id_differs', { ...inProj(F('search', { projectId: PID })), project: P([C('qa-chat-1', F('search', { projectId: PID }))], { id: 'qa-project-2' }) });
addStored('project_record_id_missing', { ...inProj(F('search', { projectId: PID })), project: { chats: [C('qa-chat-1', F('search', { projectId: PID }))] } });
addStored('project_record_id_number', { ...inProj(F('search', { projectId: '5' })), projectId: '5', project: { id: 5, chats: [C('qa-chat-1', F('search', { projectId: '5' }))] } });
// Lists that are not lists, entries that are not chats.
for (const project of ['qa-string', 7, [], {}, { id: PID }, { id: PID, chats: 'qa' }, { id: PID, chats: {} }, { id: PID, chats: null }, true, 0, '']) {
  addStored(`project_${JSON.stringify(project)}`, { ...free(F('question')), projectId: PID, project });
}
for (const freeChats of [null, {}, 'qa', 5, undefined, [null, 5, 'qa-chat-1', [], {}, { id: 5 }]]) {
  addStored(`free_chats_${JSON.stringify(freeChats)}`, { ...free(F('question')), freeChats });
}
addStored('entries_odd_then_match', { ...free(F('question')),
  freeChats: [null, 0, '', 'qa-chat-1', ['qa-chat-1'], { id: 5 }, { id: ['qa-chat-1'] }, { title: 'QA' }, C('qa-chat-1', F('question'))] });
addStored('numeric_id_vs_string', { ...free(F('question')), chatId: '5', freeChats: [{ id: 5, frame: F('search') }, { id: '5', frame: F('idea') }] });
addStored('input_empty', {});
// Tags through normalizeFrame then safeTag.
const TAG_CASES = {
  duplicates: ['qa', 'qa', '#qa', '##qa', ' qa ', 'QA', 'Ｑａ', 'qa　'],
  hashes: ['#', '##', '#qa', '# qa', 'qa#', '#　qa'],
  whitespace: ['qa notes', 'qa  notes', '\tqa\nnotes　x', '  qa﻿ ', ' qa ', 'qa᠎notes', 'qa​notes', 'qa\u0085x'],
  nfkc_width: ['Ｑａ', 'ｑａ１２３', 'ＡＢＣ', 'ｶﾞｷﾞ', 'ﾊﾟﾝ', '㌀', '㍻', '㈱'],
  nfkc_ligatures: ['ﬁ', 'ﬃle', 'ﬆ', 'ﬀ', 'Ĳ', 'ǅ', 'ʰa', 'ſt'],
  nfkc_digits: ['①②', '²³', '½', '⅞', '٣٤', 'Ⅻ', 'ⅶ', '𝟘𝟙', '⒈', '⑴'],
  punctuation: ['qa.notes!', 'qa,notes', 'qa/notes', 'qa\\notes', '"qa"', '<qa>', 'qa&b', 'qa‐notes', 'qa–notes', 'qa：x', '¿qa?'],
  emoji: ['qa😀', '😀', '👍🏽qa', '🇩🇪', '❤️qa', '#️⃣', 'qa🙂notes', '©qa'],
  astral: ['𝐀𝐁𝐂', '𐐀𐐁', '𠀀𠀁', '𞤀', 'qa𝒳', '𝔄'],
  surrogate_cut: ['x'.repeat(31) + '𐐀', 'x'.repeat(30) + '𐐀𐐁', 'x'.repeat(31) + '𠀀', 'x'.repeat(63) + '𐐀', 'x'.repeat(31) + '😀',
    'x'.repeat(31) + '𝐀', 'q'.repeat(33), 'w'.repeat(32)],
  marks: ['é', 'ä', '́qa', 'q⃝a', 'क्ष', 'ก่า', 'ño', 'á́', '가', 'qaั'],
  lone_surrogates: ['\ud83d', '\ude00', 'qa\ud83d', '\ude00qa', '\ud801\ud801'],
  more_than_six: ['qa1', 'qa2', 'qa3', 'qa4', 'qa5', 'qa6', 'qa7', 'qa8'],
  more_than_twenty: Array.from({ length: 25 }, (_, i) => `qa${i}`),
  empties_before_six: ['!', '?', '😀', '', ' ', 'qa1', 'qa2', 'qa3', 'qa4', 'qa5', 'qa6', 'qa7'],
  non_strings: [5, null, true, {}, ['qa'], 'qa', 0, false],
};
for (const [label, tags] of Object.entries(TAG_CASES)) {
  for (const kind of ['search', 'action']) {
    addStored(`tags_${label}_${kind}`, inProj(F(kind, { projectId: PID, tags })));
    addNormalize(`tags_${label}_${kind}`, F(kind, { projectId: PID, tags }));
  }
}
for (const tags of ['qa', { 0: 'qa' }, 5, null, true]) addStored(`tags_not_array_${JSON.stringify(tags)}`, free(F('idea', { tags })));
// Tag sets that force the 400-unit trimming: long tags, every kind, with and without the scope line.
const long = (c, n) => Array.from({ length: n }, (_, i) => `${String.fromCharCode(97 + i)}${c.repeat(40)}`);
for (const kind of KINDS) {
  addStored(`trim_${kind}_in_project`, inProj(F(kind, { projectId: PID, tags: long('q', 8) })));
  addStored(`trim_${kind}_free`, free(F(kind, { tags: long('w', 8) })));
  addStored(`trim_${kind}_astral`, inProj(F(kind, { projectId: PID, tags: Array.from({ length: 8 }, (_, i) => `${i}${'𐐀'.repeat(20)}`) })));
}

// normalizeFrame alone (the reuse of server-projects' port is proven on these and the tag cases above).
for (const frame of [undefined, null, 0, '', 'search', [], ['search'], {}, { kind: 'nope' }, { kind: 'search' }, { kind: 'search', tags: 'qa' },
  { kind: 'code', projectId: 'x'.repeat(128) }, { kind: 'code', projectId: 'x'.repeat(129) }, { kind: 'code', projectId: '' },
  { kind: 'code', projectId: 5 }, { kind: 'idea', confirmed: 1, source: 'user' }, { kind: 'idea', confirmed: true, source: 'User' },
  { kind: 'idea', extra: 'qa', links: ['a', 'a', 'b'], tags: [' #a  b ', 'a-b', '#', '   ', 'x'.repeat(70) + '😀', 'x'.repeat(63) + '😀'] },
  { kind: 'action', tags: Array.from({ length: 30 }, (_, i) => `t${i}`), links: Array.from({ length: 30 }, (_, i) => `l${i}`) },
  { kind: 'question', tags: ['\u0085qa\u0085', '᠎qa', '​qa', '⁠qa', ' qa ', ' qa', ' qa', ' qa'] }]) {
  addNormalize(`normalize_${JSON.stringify(frame)}`.slice(0, 80), frame);
}

// A generated corpus: lists of random chats, a random chat id and project id.
const IDS = ['qa-chat-1', 'qa-chat-2', 'qa-chat-3', 'qa-chat-4'];
const randomFrame = () => {
  if (rand() < 0.08) return pick([null, 'search', 5, [], {}]);
  const f = { kind: rand() < 0.9 ? pick(KINDS) : pick(['nope', '', 5]) };
  if (rand() < 0.9) f.confirmed = rand() < 0.75 ? true : pick([false, 'true', 1]);
  if (rand() < 0.7) f.projectId = pick([PID, 'qa-project-2', null, '', 5]);
  if (rand() < 0.8) f.tags = rand() < 0.95 ? tagsOf(int(0, 9)) : pick(['qa', 5, {}]);
  if (rand() < 0.5) f.links = rand() < 0.9 ? Array.from({ length: int(0, 4) }, () => pick([...IDS, '', 5])) : 'qa';
  if (rand() < 0.6) f.source = pick(['user', 'suggested', 'other']);
  return f;
};
const randomList = () => Array.from({ length: int(0, 4) }, () => (rand() < 0.06 ? pick([null, 5, 'qa-chat-1']) : C(pick(IDS), rand() < 0.85 ? randomFrame() : undefined)));
for (let n = 0; n < 400; n += 1) {
  const input = { enabled: rand() < 0.92, chatId: rand() < 0.93 ? pick(IDS) : pick([null, '', 5]) };
  if (rand() < 0.85) input.projectId = pick([PID, PID, 'qa-project-2', null]);
  if (rand() < 0.75) input.project = rand() < 0.9 ? P(randomList(), { id: pick([PID, PID, PID, 'qa-project-2']) }) : pick([null, 'qa', []]);
  if (rand() < 0.9) input.freeChats = rand() < 0.95 ? randomList() : pick([null, {}]);
  addStored(`random_${n}`, input);
}

// ── frameBlock and gateBias of frames given directly (tags as they come, not through cleanTag) ──────────────────
for (const kind of KINDS) {
  for (const inProject of [false, true]) {
    addBlock(`direct_${kind}_${inProject}_none`, { ...F(kind), inProject });
    addBlock(`direct_${kind}_${inProject}_unconfirmed`, { ...F(kind, { confirmed: false, tags: ['qa'] }), inProject });
    // Every count of tags of every length up to 32 units around the 400-unit edge.
    for (let count = 1; count <= 7; count += 1) {
      for (const width of [1, 8, 16, 24, 28, 30, 31, 32, 33]) {
        const tags = Array.from({ length: count }, (_, i) => `${String.fromCharCode(97 + i)}${'q'.repeat(width - 1)}`);
        addBlock(`edge_${kind}_${inProject}_${count}x${width}`, { ...F(kind, { tags }), inProject });
      }
    }
  }
}
for (let n = 0; n < 500; n += 1) {
  const tags = Array.from({ length: int(0, 10) }, () => tagOf(int(1, 5)));
  addBlock(`random_block_${n}`, { ...F(pick(KINDS), { tags, confirmed: rand() < 0.95 }), inProject: rand() < 0.5 });
}

// ── The code point sweep ────────────────────────────────────────────────────────────────────
const QHEAD = `Chat frame (set by the user): this chat is ${steering.SHAPES.question}`;
const probe = steering.frameBlock({ ...F('question', { tags: ['qaprobe'] }), inProject: false });
const [PRE, POST] = probe.split('qaprobe');
const shownTag = (block) => {
  if (block === QHEAD) return '';
  if (!block.startsWith(PRE) || !block.endsWith(POST)) throw new Error(`unexpected block ${JSON.stringify(block)}`);
  return block.slice(PRE.length, block.length - POST.length);
};
const charOf = (cp) => (cp >= 0xd800 && cp <= 0xdfff ? String.fromCharCode(cp) : String.fromCodePoint(cp));
function sweepOf(prefix) {
  const kept = [], removed = [], changed = [];
  let run = null; // [class, lo, hi]
  const flush = () => { if (run) (run[0] === 'k' ? kept : removed).push([run[1], run[2]]); run = null; };
  for (let cp = 0; cp <= 0x10ffff; cp += 1) {
    const s = prefix + charOf(cp);
    const frame = { ...F('question', { tags: [s] }), inProject: false };
    const block = steering.frameBlock(frame);
    const out = shownTag(block);
    if (cp % 4099 === 0 || [0x1d400, 0xfb01, 0xff21, 0x2460, 0x0301, 0xd83d, 0xdc00, 0x10400, 0x20000].includes(cp)) {
      addBlock(`sweep_${prefix || 'alone'}_U+${cp.toString(16).toUpperCase()}`, frame);
    }
    const cls = out === s ? 'k' : out === prefix ? 'r' : 'c';
    if (cls === 'c') { flush(); changed.push([cp, out]); continue; }
    if (run && run[0] === cls && run[2] === cp - 1) run[2] = cp;
    else { flush(); run = [cls, cp, cp]; }
  }
  flush();
  return { prefix, kept, removed, changed };
}
const sweep = [sweepOf(''), sweepOf('a')];

// Named differences: none found (the sweep and every case above replay byte for byte).
const differences = [];

// ── Output ──────────────────────────────────────────────────────────────────────────────────
const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
const text = `{"version":1,"generator":"crates/chat-prepare/tests/record_frame.cjs",\n"node":${JSON.stringify(process.version)},\n`
  + `"icu":${JSON.stringify(process.versions.icu)},\n"unicode":${JSON.stringify(process.versions.unicode)},\n`
  + `"sources":${JSON.stringify(sources)},\n"shapes":${JSON.stringify(steering.SHAPES)},\n"maxBlockChars":${steering.MAX_BLOCK_CHARS},\n`
  + `"normalize":[\n${lines(normalize)}\n],\n"stored":[\n${lines(stored)}\n],\n"blocks":[\n${lines(blocks)}\n],\n`
  + `"sweep":[\n${lines(sweep)}\n],\n"differences":[\n${lines(differences)}\n]}\n`;
const check = process.argv.indexOf('--check');
const framed = stored.filter((c) => c.frame).length;
const counts = `${normalize.length} normalize, ${stored.length} stored (${framed} framed), ${blocks.length} blocks, `
  + `${sweep.map((s) => `${s.kept.length}/${s.removed.length}/${s.changed.length}`).join(' + ')} sweep runs kept/removed/changed, ${differences.length} differences`;
if (check > 0) {
  const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
  if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
  process.stderr.write(`ok: ${counts} recorded as committed\n`);
} else {
  process.stderr.write(`recorded: ${counts}\n`);
  process.stdout.write(text);
}
