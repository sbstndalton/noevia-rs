#!/usr/bin/env node
// Test-oracle glue only: noevia-core's own recent-writes.cjs fingerprint/canonical on synthetic writes.
//   node record-writes.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const w = require(path.join(core, 'server', 'recent-writes.cjs'));
let seed = 17;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const NAMES = ['write_note', '', null, 7, 'é😀', '\ud83d', undefined];
const TARGETS = ['notes/a.md', '', null, 5, undefined, 'x"y'];
const ARGS = ['', '{}', '{"b":2,"a":1}', '{"a":1,"b":2}', '[1,{"z":[true,null],"y":"\\ud83d"}]', '{"10":1,"9":2,"a":{"B":1,"a":2}}', 'not json', '{"x":1e21,"y":-0,"z":0.1}', '"str"', '42', 'null', '{"é":1,"e":2,"😀":3,"￿":4}', 7, null, undefined, '{"a":', ' {"a":1} '];
const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), cases: [] };
for (let i = 0; i < 600; i++) {
  const name = pick(NAMES), target = pick(TARGETS), args = pick(ARGS);
  const enc = (v) => (v === undefined ? '__undefined' : v);
  out.cases.push({ name: enc(name), target: enc(target), args: enc(args), fp: w.fingerprint(name, target, args), canonical: typeof args === 'string' ? (() => { try { return w.canonical(JSON.parse(args)); } catch { return null; } })() : null });
}
fs.writeFileSync(process.argv[3], JSON.stringify(out));
console.log(`recorded ${out.cases.length}`);
