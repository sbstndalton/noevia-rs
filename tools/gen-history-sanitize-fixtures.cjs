#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own transcript sanitiser (server/chat-sources.cjs
// sanitizeHistory with chat-variants.cjs capEntry) on synthetic transcripts and records what it
// returns, or that it threw.
//
//   node tools/gen-history-sanitize-fixtures.cjs <noevia-core checkout> <output.json>
//
// crates/server-projects/tests/history_sanitize_differential.rs replays every case against the
// Rust port (crates/server-projects src/history_sanitize.rs). The cases are deterministic (a fixed
// seed), synthetic and QA-named: hand-picked edges, then generated transcripts with nested
// alternatives, sources, skill pins, failures, odd values, Unicode whitespace and lone surrogates.
'use strict';
const fs = require('node:fs');
const path = require('node:path');
const cp = require('node:child_process');

const core = path.resolve(process.argv[2] || '../noevia-core');
const out = process.argv[3];
if (!out) throw Error('usage: gen-history-sanitize-fixtures.cjs <core> <output.json>');
const sha = cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
const { sanitizeHistory } = require(path.join(core, 'server', 'chat-sources.cjs'));

let seed = 0x2071;
const rand = () => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return seed / 0x100000000; };
const pick = (xs) => xs[Math.floor(rand() * xs.length)];
const int = (n) => Math.floor(rand() * n);

const PIN = 'skill_' + 'a'.repeat(32) + '@' + 'b'.repeat(64);
const SPACES = [' ', '\t', '\n', ' ', ' ', '　', '﻿', ' ', '​'];
const words = ['qa', 'synthetic', 'note', 'alpha', '😀', '\ud83d', 'é', 'ORCHID-42'];
function text(max = 6) {
  let s = '';
  const n = int(max);
  for (let i = 0; i < n; i++) s += pick(words) + (rand() < 0.5 ? pick(SPACES) : ' ');
  if (rand() < 0.05) s = s.repeat(12);
  return s;
}
function odd() {
  return pick([null, 0, 1, -1, 2.5, true, false, '', 'x', [], {}, [1, 'a'], { toString: 1 }, { valueOf: 2 }, 1e21, 1e300, -0.0004]);
}
function source() {
  if (rand() < 0.1) return odd();
  const s = {};
  if (rand() < 0.9) s.file = rand() < 0.1 ? odd() : 'qa-' + text(3) + '.md';
  if (rand() < 0.5) s.id = rand() < 0.3 ? odd() : 'qa-id-' + int(99);
  if (rand() < 0.7) s.snippet = rand() < 0.2 ? odd() : text(rand() < 0.2 ? 80 : 8);
  if (rand() < 0.6) s.kind = pick(['excerpt', 'file', 'other', 1]);
  if (rand() < 0.5) s.score = pick([0.12345, 1, -0.0005, 0.0005, 2.5e-4, 'x', null, 1e21, 0.9999]);
  if (rand() < 0.4) s.page = pick([1, 4, 0, -1, 100000, 100001, 2.5, '3', null]);
  if (rand() < 0.2) s.extra = 'qa';
  return s;
}
function item(role, depth) {
  if (rand() < 0.05) return odd();
  const m = {};
  const keys = ['role', 'content', 'sources', 'skill', 'failure', 'model', '0', '7'];
  for (const k of keys.sort(() => rand() - 0.5)) {
    if (k === 'role') { if (rand() < 0.9) m.role = rand() < 0.1 ? odd() : role; }
    else if (k === 'content') { if (rand() < 0.9) m.content = text(); }
    else if (k === 'sources') { if (rand() < 0.4) m.sources = rand() < 0.1 ? odd() : Array.from({ length: int(14) }, source); }
    else if (k === 'skill') { if (rand() < 0.3) m.skill = pick([PIN, PIN.toUpperCase(), PIN + 'x', 'skill_x', 5, null]); }
    else if (k === 'failure') { if (rand() < 0.3) m.failure = pick(['  qa failed  ', '', '   ', 5, null, 'q'.repeat(1001), '　qa　']); }
    else if (rand() < 0.05) m[k] = 'qa';
  }
  const wide = depth === 0 ? 13 : 3;
  if (depth < 3 && rand() < 0.35) {
    if (rand() < 0.6) {
      m.answers = Array.from({ length: int(wide) }, () => item('assistant', depth + 1));
      if (rand() < 0.7) m.answer = pick([0, 1, 5, 12, 13, 20, -1, 2.5, '3', null, 1e300]);
    }
    if (rand() < 0.5) {
      m.branches = Array.from({ length: int(wide) }, () => {
        const v = item(role, depth + 1);
        if (v && typeof v === 'object' && !Array.isArray(v) && rand() < 0.6) {
          v.tail = rand() < 0.1 ? odd() : Array.from({ length: int(3) }, () => item(pick(['user', 'assistant']), depth + 1));
        }
        return v;
      });
      if (rand() < 0.7) m.branch = pick([0, 2, 9, 10, 13, -3, 'x', null]);
    }
  }
  return m;
}
function deep(n) {
  let m = { role: 'user', content: 'qa leaf', answers: [{ sources: [{ file: 'f' }] }] };
  for (let i = 0; i < n; i++) m = { role: 'user', content: 'qa ' + i, branches: [{ content: 'v', tail: [m] }] };
  return [m];
}

const hand = [
  [],
  [null, 0, 'x', [1], true],
  [{ role: 'assistant', content: 'a', sources: [{ file: 'qa.md', snippet: '  a\n\tb  ', score: 0.12345, kind: 'excerpt', page: 3 }] }],
  [{ role: 'user', content: 'q', skill: PIN }],
  [{ role: 'user', content: 'q', skill: PIN, sources: [] }],
  [{ role: 'assistant', content: '', failure: '  The reply failed.  ' }],
  [{ role: 'assistant', sources: [{ file: 'f', snippet: { toString: 1 } }] }],
  [{ role: 'assistant', sources: [{ file: 'f', snippet: [1, [2, null], 'a'] }] }],
  [{ role: 'user', branches: [{ content: 'v', skill: 'bad' }] }],
  [{ role: 'user', branches: [{ content: 'v' }] }],
  [{ role: 'user', answers: Array.from({ length: 12 }, (_, i) => i), answer: 0 }],
  [{ role: 'user', answers: Array.from({ length: 12 }, (_, i) => i), answer: 11 }],
  [{ role: 'user', answers: [1, 2], answer: 30 }],
  [{ role: 'assistant', sources: [{ file: 'f', snippet: 'é'.repeat(250) }] }],
  [{ role: 'assistant', sources: [{ file: 'f', snippet: 'a'.repeat(198) + '😀' + 'b' }] }],
  [{ role: 'assistant', sources: [{ file: 'x'.repeat(400), id: 'y'.repeat(400) }] }],
  deep(15), deep(16), deep(17), deep(18),
];

const cases = hand.map((input, i) => ({ name: `hand-${i}`, input }));
for (let i = 0; i < 400; i++) cases.push({ name: `gen-${i}`, input: Array.from({ length: int(3) + 1 }, () => item(pick(['user', 'assistant']), 0)) });

const records = cases.map(({ name, input }) => {
  const text = JSON.stringify(input);
  let output = null, threw = false;
  try { output = JSON.stringify(sanitizeHistory(JSON.parse(text))); } catch { threw = true; }
  return { name, input: text, output, threw };
});
fs.writeFileSync(out, JSON.stringify({ core: sha, cases: records }) + '\n');
console.log(`${records.length} cases from core ${sha}`);
