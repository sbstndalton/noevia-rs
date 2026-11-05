#!/usr/bin/env node
// Test-oracle glue only: noevia-core's own chat-sources.cjs buildSources/usedInjected and source-readability.cjs on
// synthetic project files and retrieval entries. Synthetic data only.
//   node record-sources.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const src = require(path.join(core, 'server', 'chat-sources.cjs'));
const read = require(path.join(core, 'server', 'source-readability.cjs'));
let seed = 23;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const NAMES = ['notes.md', 'report.pdf', 'data.csv', 'x'.repeat(310), 'é😀.txt', 7];
const BODIES = ['', '[Page 4]\nThe total is ORCHID-42.', '  [Page 12] tail', '[Page 0]\nx', '[Page 1234567]\n', 'red, orange, yellow and green apples grow here', 'INV-2042 costs 34.95 today', 'a\n\n b\t c  ' + 'word '.repeat(60), ' [Page 3]', null, 5, 'Über 2024 großartig ÉTÉ-9 x1y2', '[page 2]\n'];
function file() {
  const f = { name: pick(NAMES), content: pick(['', '  ', 'text']) };
  if (rnd(3) === 0) f.attachment = { id: pick(['att-1', '', 9, undefined]), state: pick(['stored', 'ready']), group: pick(['Text', 'Documents', 'Images']) };
  if (rnd(3) === 0) f.document = { state: pick(['failed', 'queued', 'ready']) };
  return JSON.parse(JSON.stringify(f));
}
const entry = () => JSON.parse(JSON.stringify({ file: pick(NAMES), body: pick(BODIES), kind: pick(['excerpt', 'file', 'other']), score: pick([0.12345, 1, -0.0005, 0.0005, 2.5e-4, Infinity, '0.5', undefined, 0.1234995]) }));
const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), build: [], used: [], files: [] };
for (let i = 0; i < 500; i++) {
  const files = Array.from({ length: rnd(6) }, file), entries = Array.from({ length: rnd(16) }, entry);
  out.build.push({ entries, files, out: src.buildSources(entries, files) });
}
for (let i = 0; i < 300; i++) { const f = file(); out.files.push({ f, readable: read.readable([f]).length === 1 }); }
const ANSWERS = ['', '  ', 'The code is ORCHID-42.', 'It costs 34.95 today at INV-2042', 'red, orange, yellow and green apples grow here', 'Über 2024 großartig', 'nothing distinctive'];
for (let i = 0; i < 500; i++) {
  const injected = Array.from({ length: rnd(5) }, () => ({ ...entry(), kind: rnd(5) ? 'file' : 'excerpt' })), placed = Array.from({ length: rnd(3) }, () => entry());
  const answer = pick(ANSWERS), question = pick(['', 'what is ORCHID-42?', 'which apples grow here', 'tell me']);
  out.used.push({ injected, placed, answer, question, out: src.usedInjected(injected, placed, answer, question) });
}
fs.writeFileSync(process.argv[3], JSON.stringify(out));
console.log(`build ${out.build.length}, used hits ${out.used.filter((u) => u.out.length).length}`);
