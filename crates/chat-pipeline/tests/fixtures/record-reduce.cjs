#!/usr/bin/env node
// Test-oracle glue only: noevia-core's own tool-result-reduce.cjs on synthetic tool results (listings, nested JSON,
// prose) at several budgets. Synthetic data only.
//   node record-reduce.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const { reduceToolResult } = require(path.join(core, 'server', 'tool-result-reduce.cjs'));
let seed = 43;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const VAL = () => pick(['', null, 0, false, 'path/to/file.md', 'x'.repeat(400), 'a\tb\nc', '😀é', 12.5, [], {}, [1, 2], { k: 'v' }, '\ud83d', 'y'.repeat(310)]);
const KEYS = ['name', 'path', 'size', 'mtime', 'etag', 'empty', 'k'.repeat(90), '10', '2', 'é', 'Content-Type'];
function row() { const r = {}; for (let n = rnd(7); n > 0; n--) r[pick(KEYS)] = VAL(); return r; }
const results = [];
for (let i = 0; i < 200; i++) {
  const kind = rnd(5);
  let v;
  if (kind <= 1) v = JSON.stringify(Array.from({ length: rnd(60) }, row));
  else if (kind === 2) v = JSON.stringify({ items: Array.from({ length: rnd(20) }, row), meta: { empty: '', none: null, list: [], note: 'x'.repeat(500) } });
  else if (kind === 3) v = 'prose line '.repeat(rnd(800)) + pick(['', '😀']);
  else v = JSON.stringify(Array.from({ length: rnd(40) }, () => pick([row(), 5, 'str'])));
  results.push(v);
}
const many = {}; for (let k = 0; k < 120; k++) many[`key_${k}`] = k % 3 ? '' : 'v'; results.push(JSON.stringify([many, many, { ...many, key_1: 'x' }]));
const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), texts: results, cases: [] };
for (const [t, text] of results.entries()) for (const maxChars of [0, 120, 400, 1500, 8000]) {
  const maxValueChars = pick([0, 20, 300]);
  const r = reduceToolResult(text, { maxChars, maxValueChars });
  out.cases.push({ t, maxChars, maxValueChars, reduced: r.reduced, note: r.note, text: r.reduced ? r.text : null });
}
fs.writeFileSync(process.argv[3], JSON.stringify(out));
console.log(`recorded ${out.cases.length}; reduced ${out.cases.filter((c) => c.reduced).length}`);
