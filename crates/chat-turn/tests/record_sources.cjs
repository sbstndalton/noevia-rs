#!/usr/bin/env node
'use strict';
// Recorded #1793 attributions for crates/chat-turn: the contract src/sources.rs is replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-turn/tests/record_sources.cjs > crates/chat-turn/tests/fixtures/node-sources.v1.json
//   ... node crates/chat-turn/tests/record_sources.cjs --check crates/chat-turn/tests/fixtures/node-sources.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every case is what chat.cjs's close does with one reply: core's own rust-chat-native.cjs injectedSources
// (the part done before the hand-off: `prepared`, as the frame carries it), then chat-sources.cjs usedInjected
// and buildSources over [...placed, ...used] (`used`: the files the answer drew on; `sources`: the list sent
// again, or null when it adds nothing, as `sources.length > replySources.length` decides). The inputs are
// synthetic and QA-named: hand-made edges (digit tokens, four-word runs, the question's own words, case
// mapping, Unicode letters and digits, the 24,000-unit cut, lone surrogates, unreadable and repeated files,
// the twelve-source cap) and seeded random mixes. The source files' sha256 are recorded.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const SOURCES = ['server/chat-sources.cjs', 'server/rust-chat-native.cjs', 'server/source-readability.cjs'];
const root = path.resolve(checkout);
const sources = Object.fromEntries(SOURCES.map((f) => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex')]));
const chatSources = require(path.join(root, 'server/chat-sources.cjs'));
const { injectedSources } = require(path.join(root, 'server/rust-chat-native.cjs'));

const cases = [];
// `files`: the readable list retrieval read from (some unreadable); `placed`/`injected`: what it put in the prompt.
function add(name, { files, placed = [], injected, answer, question = 'QA question?' }) {
  const reply = chatSources.buildSources(placed, files);
  const prepared = injectedSources({ injected, placed, readable: files, reply });
  const used = chatSources.usedInjected(injected, placed, answer, question);
  const again = chatSources.buildSources([...placed, ...used], files);
  cases.push({ name, prepared, answer, question, used: used.map((e) => e.file), sources: again.length > reply.length ? again : null });
}
const file = (name, extra = {}) => ({ name, content: `QA content of ${name}`, ...extra });
const whole = (name, body) => ({ file: name, body, kind: 'file' });
const one = (name, body, answer, question) => add(name, { files: [file('qa-a.txt')], injected: [whole('qa-a.txt', body)], answer, question });

// ── Tokens and runs ─────────────────────────────────────────────────────────────────────────
one('digit_token', 'The QA code is ORCHID-42.', 'Your code is orchid-42.');
one('digit_token_in_question', 'The QA code is ORCHID-42.', 'Your code is ORCHID-42.', 'Is it ORCHID-42?');
one('decimal_token', 'QA total 34.95 due', 'It costs 34.95.');
one('four_digits_no_letter', 'QA year 2042 only', 'In 2042.');
one('five_digits_no_letter', 'QA ref 20425 only', 'Ref 20425.');
one('four_with_letter', 'QA id ab12', 'id AB12');
one('three_with_letter', 'QA id a12', 'id a12');
one('no_digit_word', 'QA extraordinary', 'extraordinary');
one('four_word_run', 'the quick brown fox jumps', 'I saw the quick brown fox today');
one('three_word_run', 'the quick brown', 'the quick brown');
one('run_in_question', 'the quick brown fox jumps', 'the quick brown fox', 'did the quick brown fox run?');
one('run_across_punctuation', 'the quick, brown; fox!', 'The Quick... Brown fox');
one('joined_words', 'QA inv-2042.b7 x', 'see INV-2042.B7 please');
one('joined_double_dash', 'QA a--b1234', 'a--b1234');
one('trailing_dot', 'QA token qa-77x.', 'qa-77x');
one('blank_answer', 'QA ORCHID-42', ' \n\t   ');
one('empty_body', '', 'anything 12345');
one('null_body', null, 'null 12345');

// ── Unicode ─────────────────────────────────────────────────────────────────────────────────
one('greek_final_sigma', 'ΟΔΟΣ ΑΘΗΝΑΣ 12 ΑΒ12Σ', 'οδος αθηνας 12 αβ12σ');
one('greek_sigma_before_dot', 'QA ΑΒΓΣ.Δ1', 'αβγσ.δ1');
one('greek_sigma_final_before_dot', 'QA ΑΒΓΣ.1', 'αβγς.1');
one('dotted_capital_i', 'İSTANBUL-34 QA', 'i̇stanbul-34');
one('dotted_capital_i_split', 'İSTANBUL-34 QA', 'stanbul-34');
one('arabic_indic_digits', 'QA رقم ٣٤٥٦٧', 'رقم ٣٤٥٦٧');
one('fullwidth_digits', 'QA １２３４５', '１２３４５');
one('roman_numeral_letter', 'QA ⅫⅫ12', 'ⅻⅻ12');
one('superscript_digit', 'QA x²²²²', 'x²²²²');
one('astral_letters', 'QA 𝐀𝐁𝐂1 notes', '𝐀𝐁𝐂1');
one('astral_case', 'QA 𐐀𐐁12', '𐐨𐐩12');
one('emoji_between', 'QA 😀ab12😀', 'ab12');
one('combining_mark', 'QA café-12', 'café-12');
one('cjk_run', '日本語 の 文書 です', '日本語 の 文書 です');
one('lone_surrogates', 'QA ab12\ud83d tail \ude00cd34', 'ab12 and cd34');
one('german_sharp_s', 'QA STRASSE-12 straße-34', 'strasse-12');
one('kelvin_sign', 'QA K-12345', 'k-12345');

// ── The 24,000-unit cut ─────────────────────────────────────────────────────────────────────
const MAX = chatSources.ATTRIBUTE_MAX;
one('token_before_cut', 'x '.repeat((MAX - 20) / 2) + 'qa-cut-91', 'qa-cut-91');
one('token_after_cut', 'x '.repeat(MAX / 2) + 'qa-cut-92', 'qa-cut-92');
one('token_straddles_cut', 'x'.repeat(MAX - 5) + ' qa1234567', 'qa12');
one('astral_at_cut', 'x'.repeat(MAX - 1) + '😀 ab12', 'ab12');
one('answer_after_cut', 'QA zz-9876', 'x '.repeat(MAX / 2) + 'zz-9876');
one('question_after_cut', 'QA zz-9877', 'zz-9877', 'x '.repeat(MAX / 2) + 'zz-9877');

// ── Files: placed, unreadable, repeated, excerpts, the cap ──────────────────────────────────
add('placed_file_skipped', { files: [file('qa-a.txt'), file('qa-b.txt')], placed: [{ file: 'qa-a.txt', body: 'QA AA-11', kind: 'excerpt', score: 0.9 }],
  injected: [whole('qa-a.txt', 'QA AA-11'), whole('qa-b.txt', 'QA BB-22')], answer: 'AA-11 and BB-22' });
add('unreadable_used_adds_nothing', { files: [file('qa-a.txt', { content: '', document: { state: 'failed' } })], injected: [whole('qa-a.txt', 'QA AA-11')], answer: 'AA-11' });
add('unread_then_readable', { files: [file('qa-a.txt', { attachment: { state: 'stored', group: 'Text' } }), file('qa-b.txt', { attachment: { id: 'qa-att-b' } })],
  injected: [whole('qa-a.txt', 'QA AA-11'), whole('qa-b.txt', 'QA BB-22')], answer: 'AA-11 BB-22' });
add('repeated_file_counts_once', { files: [file('qa-a.txt')], injected: [whole('qa-a.txt', 'QA nothing here'), whole('qa-a.txt', 'QA AA-11'), whole('qa-a.txt', 'QA AA-12')],
  answer: 'AA-11 AA-12' });
add('excerpt_entries_ignored', { files: [file('qa-a.txt')], injected: [{ file: 'qa-a.txt', body: 'QA AA-11', kind: 'excerpt' }], answer: 'AA-11' });
add('not_in_files', { files: [file('qa-a.txt')], injected: [whole('qa-z.txt', 'QA ZZ-11')], answer: 'ZZ-11' });
add('page_and_score', { files: [file('qa-a.txt'), file('qa-b.txt')], placed: [{ file: 'qa-a.txt', body: '[Page 7]\nQA', kind: 'excerpt', score: 0.123456 }],
  injected: [whole('qa-b.txt', '[Page 2]\nQA BB-22 ' + 'long text '.repeat(40))], answer: 'BB-22' });
{
  const files = Array.from({ length: 16 }, (_, i) => file(`qa-${i}.txt`));
  const placed = files.slice(0, 10).map((f, i) => ({ file: f.name, body: `QA placed ${i}`, kind: 'excerpt', score: i / 10 }));
  const injected = files.slice(10).map((f, i) => whole(f.name, `QA ID-${100 + i}`));
  add('cap_twelve', { files, placed, injected, answer: 'ID-100 ID-101 ID-102 ID-103 ID-104' });
  const full = files.slice(0, 12).map((f, i) => ({ file: f.name, body: `QA placed ${i}`, kind: 'excerpt' }));
  add('reply_already_full', { files, placed: full, injected: files.slice(12).map((f, i) => whole(f.name, `QA ID-${200 + i}`)), answer: 'ID-200 ID-201' });
}

// ── Seeded mixes ────────────────────────────────────────────────────────────────────────────
let seed = 0x1793_2026;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const pick = (list) => list[Math.floor(rand() * list.length)];
const WORDS = ['qa', 'report', 'folder', 'notes', 'Ünïcode', 'naïve', '日本語', '😀', '𝒳qa', 'red', 'orange', 'the', 'and', 'ΟΔΟΣ', 'İzmir',
  'tab\there', 'line\nbreak', ' ', 'é', '—', 'INV', 'ORCHID', 'x', 'Straße', 'ǅemal', 'ﬁle', 'ΣΑΣ'];
const token = () => pick([`${pick(WORDS)}-${int(1, 99999)}`, `${int(10, 99)}.${int(10, 99)}`, `${int(1000, 999999)}`, `qa${int(1, 999)}`, `${pick(WORDS)}${int(0, 9)}`]);
const text = (n) => Array.from({ length: n }, () => (rand() < 0.2 ? token() : pick(WORDS))).join(pick([' ', ' ', ', ', '. ', '\n']));
for (let i = 0; i < 160; i += 1) {
  const count = int(1, 5);
  const files = Array.from({ length: count + 1 }, (_, k) => file(`qa-mix-${i}-${k}.txt`, k === count && rand() < 0.5 ? { content: '', document: { state: 'queued' } } : {}));
  const bodies = files.map(() => text(int(0, 40)));
  const injected = files.map((f, k) => whole(rand() < 0.1 ? files[0].name : f.name, bodies[k]));
  const placed = rand() < 0.3 ? [{ file: files[0].name, body: bodies[0].slice(0, 30), kind: 'excerpt', score: rand() }] : [];
  const quote = (b) => { const w = b.split(/\s+/); const at = int(0, Math.max(0, w.length - 5)); return w.slice(at, at + int(1, 6)).join(' '); };
  const answer = [text(int(0, 8)), ...bodies.filter(() => rand() < 0.5).map(quote), text(int(0, 5))].join(' ');
  const question = rand() < 0.4 ? quote(pick(bodies)) : text(int(0, 6));
  add(`mix_${i}`, { files, placed, injected, answer, question });
}

// ── Output ──────────────────────────────────────────────────────────────────────────────────
const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
const out = `{"version":1,"generator":"crates/chat-turn/tests/record_sources.cjs (noevia-core chat-sources.cjs usedInjected + buildSources, rust-chat-native.cjs injectedSources)",\n`
  + `"sources":${JSON.stringify(sources)},\n"attributeMax":${MAX},\n"cases":[\n${lines(cases)}\n]}\n`;
const check = process.argv.indexOf('--check');
if (check > 0) {
  const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
  if (committed !== out) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
  process.stderr.write(`ok: ${cases.length} attributions recorded as committed\n`);
} else process.stdout.write(out);
