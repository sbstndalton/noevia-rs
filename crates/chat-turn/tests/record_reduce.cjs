#!/usr/bin/env node
'use strict';
// Recorded tool-result reductions for crates/chat-turn: the contract src/reduce.rs is replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-turn/tests/record_reduce.cjs > crates/chat-turn/tests/fixtures/node-reduce.v1.json
//   ... node crates/chat-turn/tests/record_reduce.cjs --check crates/chat-turn/tests/fixtures/node-reduce.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every case calls noevia-core's own server/tool-result-reduce.cjs reduceToolResult(input, { maxChars })
// and keeps JSON.stringify of the { text, reduced, note } it returns, so the replay compares bytes. The
// inputs are synthetic and QA-named, built from a seeded PRNG: prose, uniform listings (empty columns, keys
// past the legend and header budgets, values with tabs, newlines, long strings and nested JSON), other
// JSON, scalars, astral characters and lone surrogates at every cut, over several maxChars down to 1.
// `inputs` holds each input once; a case names its input by index. `differences` are inputs JSON.parse
// accepts and the Rust parser refuses: Node's output is recorded (non-ASCII written as \u escapes), and the replay checks the documented
// Rust behaviour instead. The source file's sha256 is recorded, so a changed Node file needs a fresh recording.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const SOURCE = 'server/tool-result-reduce.cjs';
const sourcePath = path.join(path.resolve(checkout), SOURCE);
const sources = { [SOURCE]: crypto.createHash('sha256').update(fs.readFileSync(sourcePath)).digest('hex') };
const { reduceToolResult } = require(sourcePath);

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
const WORDS = ['QA', 'qa-alpha', 'report', 'folder', 'notes', 'Ünïcode', 'naïve', '日本語', '😀', '𝒳qa', 'tab\there', 'line\nbreak',
  'cr\r\nlf', 'run\t\t\n\r end', 'quote"qa', 'back\\slash', '\u2028sep', 'é', '—', '\u00a0nbsp'];
const words = (n) => Array.from({ length: n }, () => pick(WORDS)).join(' ');
const prose = (units) => { let s = ''; while (s.length < units) s += `QA paragraph ${int(1, 999)}: ${words(12)}.\n`; return s.slice(0, units); };
const pad = (text, units) => text + ' '.repeat(Math.max(0, units - text.length));

const inputs = [];
const cases = [];
const differences = [];
const MAXES = [0, 8000, 4000, 2000, 1000, 500, 300, 200, 150, 120, 100, 81, 80, 60, 40, 20, 10, 5, 3, 2, 1];

function addInput(text) { inputs.push(text); return inputs.length - 1; }
function record(input, maxChars) {
  return JSON.stringify(reduceToolResult(inputs[input], { maxChars }));
}
// One input under every cap below its length (where something happens), and one cap at or over it.
function add(name, text, maxes = MAXES) {
  const input = addInput(text);
  const cap = (m) => m || 8000; // reduceToolResult's `opts.maxChars || DEFAULT_MAX_CHARS`
  const chosen = maxes.filter((m) => cap(m) < text.length);
  // One unit under the input's length: reduced, with the most room the branch ever gets.
  if (text.length > 1 && !maxes.some((m) => cap(m) === text.length - 1)) chosen.push(text.length - 1);
  const over = maxes.find((m) => cap(m) >= text.length);
  if (over !== undefined) chosen.push(over);
  for (const maxChars of chosen) cases.push({ name: `${name}@${maxChars}`, maxChars, input, expect: record(input, maxChars) });
}
function difference(name, about, text, maxChars) {
  const input = addInput(text);
  // Every non-ASCII unit escaped (the same JSON): Node's output here may hold a character the port's parser refuses.
  const node = record(input, maxChars).replace(/[^\x00-\x7f]/g, (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, '0')}`);
  differences.push({ name, about, maxChars, input, node });
}

// ── Pass-through and prose ──────────────────────────────────────────────────────────────────
add('empty', '', [0, 1]);
add('short', 'QA short reply', [13, 14, 15, 0]);
add('exactly_default', 'q'.repeat(8000), [0]);
add('one_over_default', 'q'.repeat(8001), [0]);
add('prose', prose(3000));
add('prose_long', prose(9000), [0, 8000, 2000]);
for (let p = 0; p < 3; p += 1) add(`astral_prose_${p}`, 'x'.repeat(p) + '😀'.repeat(300) + 'qa', [7, 8, 99, 100, 101, 1]);
add('lone_surrogates_prose', `\ud83d QA lone high, \ude00 lone low, ${'😀\ud83d'.repeat(50)}`, [1, 2, 3, 40, 41, 100]);
add('not_json_bracket', `[{"qaId":1},${'{"qaId":2},'.repeat(30)}`, [10, 100]);
add('not_json_trailing_comma', pad('{"qa": 1,}', 60), [10, 50]);
add('not_json_bom', `\ufeff${JSON.stringify([{ qa: 1 }, { qa: 2 }])}${' '.repeat(40)}`, [10, 40]);
add('not_json_nan', pad('NaN', 50), [10]);
add('not_json_single_quotes', pad("{'qa': 'x'}", 50), [10]);
add('not_json_lone_after_backslash', pad('["\\\ud83d"]', 50), [10]);
add('not_json_lone_outside_string', pad('[\ud83d]', 50), [10]);

// ── Scalars ─────────────────────────────────────────────────────────────────────────────────
add('scalar_null', pad('null', 200), [10, 100, 150]);
add('scalar_true', pad(' true', 200), [10, 150]);
add('scalar_negative_zero', pad('-0', 200), [10, 150]);
for (const n of ['1e21', '0.1', '5e-7', '123456789012345678901', '-1.5E+300', '1e-400', '2.5']) add(`scalar_number_${n}`, pad(n, 200), [10, 150]);
add('scalar_string_long', JSON.stringify(prose(1200)), [100, 400, 1000]);
for (let p = 0; p < 3; p += 1) add(`scalar_string_astral_${p}`, JSON.stringify('q'.repeat(p) + '😀'.repeat(400)), [50, 151, 152, 400]);
add('scalar_string_escaped_lone', JSON.stringify(`QA ${'\ud83d'.repeat(5)} ${'x'.repeat(320)}`), [30, 200]);
add('scalar_empty_array', pad('[]', 200), [10, 150]);
add('scalar_empty_object', pad('{}', 200), [10, 150]);

// ── Uniform listings ────────────────────────────────────────────────────────────────────────
const NAMES = ['QA report', 'QA folder', 'QA notes', 'QA 😀 photo', 'QA 日本語', 'QA tab\tname', 'QA multi\n\nline'];
function listing(count, shape = {}) {
  const rows = [];
  for (let i = 0; i < count; i += 1) {
    const row = {
      qaId: i + 1,
      qaName: `${pick(NAMES)} ${i}`,
      qaPath: `/QA/${words(int(1, 4)).replace(/\s+/g, '-')}/${i}.txt`,
      qaSize: int(0, 1e6) * (rand() < 0.1 ? -0.5 : 1),
      qaEmpty: '',
      qaNull: null,
      qaTags: [],
      qaMeta: {},
      qaFlag: rand() < 0.5,
      qaZero: 0,
    };
    if (rand() < 0.4) row.qaSometimes = words(3); else if (rand() < 0.5) row.qaSometimes = '';
    if (shape.long) row.qaBody = 'x'.repeat(i % 3) + prose(int(250, 600));
    if (shape.nested) row.qaNested = { qaLevel: { qaDeep: [1, 'two', null, { qa: words(int(1, 80)) }] }, qaList: Array.from({ length: int(0, 40) }, (_, j) => j) };
    if (shape.whitespace) row.qaText = `QA\t\tcol\r\nnext\n\n\tline ${words(5)}\r`;
    if (shape.numbers) Object.assign(row, { qaFloat: rand() * 1e-7, qaBig: 2 ** 60 + i, qaExp: 1e21 * (i + 1), qaNeg0: -0 });
    if (shape.sparse && i % 2) { delete row.qaName; delete row.qaFlag; }
    rows.push(row);
  }
  return rows;
}
const json = (v) => JSON.stringify(v);
add('listing_2', json(listing(2)));
add('listing_3', json(listing(3)));
add('listing_12', json(listing(12)));
add('listing_12_pretty', JSON.stringify(listing(12), null, 2), [0, 2000, 500, 100]);
add('listing_long_values', json(listing(8, { long: true })), [0, 4000, 2000, 1000, 400, 100]);
add('listing_nested', json(listing(10, { nested: true })), [0, 4000, 2000, 600, 100]);
add('listing_whitespace', json(listing(10, { whitespace: true })), [0, 2000, 900, 300]);
add('listing_numbers', json(listing(10, { numbers: true })), [0, 2000, 900, 300]);
add('listing_sparse', json(listing(10, { sparse: true })), [0, 2000, 900, 300]);
add('listing_500', json(listing(500)), [0, 8000, 4000, 1000]);
add('listing_150_long', json(listing(150, { long: true, nested: true })), [0, 8000]);
for (let p = 0; p < 4; p += 1) {
  // A value whose 300-unit cut lands on, before and after an astral character.
  add(`listing_value_astral_${p}`, json([{ qaBody: 'x'.repeat(297 + p) + '😀'.repeat(20) }, { qaBody: `${'😀'.repeat(160)}` }, { qaBody: 'short' }]), [100, 200, 400, 1000]);
}
add('listing_keys_all_empty', json([{ qaA: '', qaB: null }, { qaA: [], qaB: {} }]) + ' '.repeat(60), [20, 60]);
add('listing_one_row', json([{ qaId: 1, qaName: prose(200) }]), [50, 150]);
add('listing_not_all_objects', json([{ qaId: 1, qaName: prose(100) }, [1, 2], { qaId: 3 }]), [50, 150]);
add('listing_with_null_row', json([{ qaId: 1, qaName: prose(100) }, null]), [50, 150]);
add('listing_duplicate_keys', `[{"qaA":1,"qaB":"x","qaA":""},{"qaB":"y","qaA":2,"qaB":""}]${' '.repeat(600)}`, [20, 60, 100]);
// Raw text, so the array-index keys come after named ones: Object.keys puts them first, ascending.
add('listing_integer_keys', `[{"qaB":1,"10":"ten","2":"two","qaA":"a","01":"x"},{"2":"zwei","qaZ":"z","4294967295":"big","4294967294":"max"}]${' '.repeat(600)}`, [20, 60, 100]);
add('listing_inherited_names', json([
  { constructor: '', toString: 'QA own', __proto__x: 1, hasOwnProperty: '', valueOf: null, qaId: 1 },
  { qaId: 2, hasOwnProperty: '' },
  { qaId: 3, isPrototypeOf: '', hasOwnProperty: null },
]) + ' '.repeat(600), [30, 100, 150, 300]);
add('listing_proto_key', `[{"__proto__":{"qa":1},"qaId":1},{"qaId":2},{"__proto__":"QA proto","qaId":3}]${' '.repeat(600)}`, [30, 100, 200]);
add('listing_proto_empty', `[{"__proto__":"","qaId":1},{"qaId":2,"constructor":""},{"qaId":3}]${' '.repeat(600)}`, [30, 100, 200]);
add('listing_tab_in_key', json([{ 'qa\tkey': 1, 'qa\nkey': 'v\tw' }, { 'qa\tkey': 2, 'qa\nkey': '' }]) + ' '.repeat(600), [20, 80]);
{
  // More than MAX_LEGEND_KEYS empty columns, and keys past the 80-unit legend cut.
  const row = { qaId: 1 };
  for (let k = 0; k < 70; k += 1) row[`qaEmptyColumn${k}`] = '';
  add('listing_70_empty_keys', json([row, { ...row, qaId: 2 }]), [0, 8000, 2000, 1000, 400, 100]);
  const wide = {};
  for (let k = 0; k < 70; k += 1) wide[`qaColumn${k}`] = `v${k}`;
  add('listing_70_full_keys', json([wide, wide, wide]), [0, 2000, 1000, 400, 200, 100, 40, 10, 3]);
  for (let p = 0; p < 3; p += 1) {
    const longKey = (c) => `qa${'k'.repeat(76 + p)}${'😀'.repeat(3)}${c}`;
    const keyed = {};
    for (let k = 0; k < 6; k += 1) keyed[longKey(k)] = k % 2 ? '' : `QA value ${k}`;
    add(`listing_long_keys_${p}`, json([keyed, keyed]), [2000, 800, 400, 100, 50]);
  }
  // Thousands of distinct keys, every one empty in every row: the legend must stay bounded.
  const many = [];
  for (let r = 0; r < 300; r += 1) {
    const m = { qaId: r };
    for (let k = 0; k < 10; k += 1) m[`qaSparse${r}_${k}`] = '';
    many.push(m);
  }
  add('listing_3000_empty_keys', json(many), [0, 8000, 2000]);
}
for (let p = 0; p < 3; p += 1) {
  // Clamp cuts (room) landing inside an astral pair.
  const rows = [{ qaName: 'q'.repeat(p) + '😀'.repeat(200) }, { qaName: '😀'.repeat(200) }];
  add(`listing_clamp_astral_${p}`, json(rows), [200, 201, 202, 203, 250, 260, 400, 500]);
}
for (let p = 0; p < 2; p += 1) {
  const rows = [{ qaName: `${'q'.repeat(p)}\ud83d${'😀\ude00'.repeat(120)}` }, { qaName: '\ude00'.repeat(150) }];
  add(`listing_lone_surrogates_${p}`, json(rows), [100, 200, 250, 600]);
  // The same strings with the lone surrogates written raw in the JSON text, not as escapes.
  add(`listing_raw_lone_surrogates_${p}`, `[{"qaName":"${rows[0].qaName}"},{"qaName":"${rows[1].qaName}"}]`, [100, 200, 250]);
}
add('listing_mixed_raw_escaped_pair', `[{"qaName":"\ud83d\\ude00${'x'.repeat(60)}"},{"qaName":"\\ud83d\ude00"}]`, [20, 60]);
for (let n = 0; n < 6; n += 1) add(`listing_random_${n}`, json(listing(int(4, 40), { long: rand() < 0.5, nested: rand() < 0.5, whitespace: rand() < 0.5, sparse: rand() < 0.5 })));

// ── Other JSON (compacted) ──────────────────────────────────────────────────────────────────
const doc = () => ({
  qaTitle: `QA document ${words(4)}`,
  qaEmpty: '', qaNull: null, qaList: [], qaObj: {},
  qaKeep: [0, false, '', null, [], {}],
  qaNested: { qaInner: { qaBlank: '', qaText: prose(int(100, 500)), qaDeeper: { qaE: {} } }, qaArr: [{ qaX: '' }, { qaY: 'y' }] },
  qaNumbers: [-0, 1e21, 0.1, 5e-7, 2 ** 53 + 2],
  qaBody: prose(700),
});
add('compact_object', json(doc()), [0, 8000, 2000, 1000, 600, 100, 10]);
add('compact_pretty', JSON.stringify(doc(), null, 2), [0, 2000, 1500, 1000]);
add('compact_mixed_array', json([doc(), 'QA string', 42, null, [doc()]]), [0, 4000, 1000, 100]);
add('compact_proto', `{"__proto__":{"qa":"x"},"qaKeep":"y","qaPad":"${'p'.repeat(400)}","qaInner":{"__proto__":"QA","qaZ":1}}`, [50, 400, 500]);
add('compact_array_of_arrays', json(Array.from({ length: 30 }, (_, i) => [i, words(3), '', null])), [100, 600, 1000]);
for (let p = 0; p < 3; p += 1) add(`compact_astral_cut_${p}`, json({ qaA: 'q'.repeat(p) + '😀'.repeat(400), qaB: '' }), [200, 201, 202, 650]);
add('compact_escaped_lone', json({ qaA: `${'\ud83d'.repeat(10)}${'x'.repeat(320)}`, qaB: '\ude00' }), [50, 300, 400]);
add('compact_raw_lone', `{"qaA":"\ud83d${'x'.repeat(320)}","qaB":"\ude00"}`, [50, 300, 400]);
{
  let deep = '"QA bottom"';
  for (let d = 0; d < 127; d += 1) deep = d % 2 ? `[${deep},""]` : `{"qa${d}":${deep},"qaE":""}`;
  add('compact_deep_127', deep, [100, 1000, 3000]);
}

// ── Differences (JSON.parse accepts, the Rust parser refuses) ───────────────────────────────
difference('overflow_number_listing', 'A number outside the f64 range: JSON.parse reads Infinity and the listing is tabulated.',
  `[{"qaId":1,"qaSize":1e400},{"qaId":2,"qaSize":-1e400}]${' '.repeat(80)}`, 120);
difference('overflow_number_compact', 'A number outside the f64 range in other JSON: Node compacts it to null.',
  `{"qaSize":1e400,"qaEmpty":""}${' '.repeat(40)}`, 30);
{
  let deep = '"QA bottom"';
  for (let d = 0; d < 200; d += 1) deep = `[${deep}]`;
  difference('nesting_200', 'Nesting deeper than 128 levels: Node compacts it.', `${deep}${' '.repeat(40)}`, 100);
}
difference('escaped_plane_16_tail', 'An escaped character of U+10F800..U+10FFFF (held as a lone surrogate by the port).',
  `{"qa":"\\udbfe\\udc00","qaEmpty":""}${' '.repeat(40)}`, 20);

// ── Output ──────────────────────────────────────────────────────────────────────────────────
const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
const text = `{"version":1,"generator":"crates/chat-turn/tests/record_reduce.cjs (noevia-core ${SOURCE} reduceToolResult)",\n`
  + `"sources":${JSON.stringify(sources)},\n"inputs":[\n${lines(inputs)}\n],\n"cases":[\n${lines(cases)}\n],\n`
  + `"differences":[\n${lines(differences)}\n]}\n`;
const check = process.argv.indexOf('--check');
if (check > 0) {
  const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
  if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
  process.stderr.write(`ok: ${cases.length} reductions and ${differences.length} differences recorded as committed\n`);
} else process.stdout.write(text);
