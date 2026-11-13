#!/usr/bin/env node
'use strict';
// R6 oracle recorder driver: runs noevia-core's llamacpp-full-autotune.test.cjs in-process under
// trace-preload.cjs and writes one trace per createFullAutotuner instance into <out>.
//   node record-node.cjs <core checkout> <out dir>             record (replaces <out>/NNN-*.json)
//   node record-node.cjs <core checkout> --baseline            same test file, no preload, no traces
// Prints the node:test pass/fail/skip counts; exits non-zero when the run fails.
const fs = require('node:fs'), path = require('node:path'), { spawnSync } = require('node:child_process');

const [coreArg, outArg] = process.argv.slice(2);
if (!coreArg || !outArg) { console.error('usage: record-node.cjs <core checkout> <out dir | --baseline>'); process.exit(2); }
const core = path.resolve(coreArg);
const testFile = path.join(core, 'server', 'llamacpp-full-autotune.test.cjs');
if (!fs.existsSync(testFile)) { console.error('no test file at ' + testFile); process.exit(2); }
const baseline = outArg === '--baseline';
const out = baseline ? null : path.resolve(outArg);
const preload = path.join(__dirname, 'trace-preload.cjs');

if (out) {
  fs.mkdirSync(out, { recursive: true });
  for (const name of fs.readdirSync(out)) if (/^\d{3}-.*\.json$/.test(name)) fs.rmSync(path.join(out, name));
}
const env = { ...process.env, TZ: 'UTC', NODE_TEST_CONTEXT: undefined };
delete env.NODE_TEST_CONTEXT;
delete env.NOEVIA_AUTOTUNE_ALLOW_BELOW_Q5_KV;
if (out) Object.assign(env, { TRACE_CORE: core, TRACE_OUT: out });
const args = [...(baseline ? [] : ['--require', preload]), '--test-reporter=tap', testFile];
const r = spawnSync(process.execPath, args, { cwd: core, env, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024, timeout: 1500000 });
const tap = (r.stdout || '') + (r.stderr || '');
if (process.env.TRACE_TAP) fs.writeFileSync(process.env.TRACE_TAP, tap);
const count = key => Number((new RegExp('^# ' + key + ' (\\d+)$', 'm').exec(tap) || [])[1] ?? NaN);
const counts = { tests: count('tests'), pass: count('pass'), fail: count('fail'), skipped: count('skipped'), todo: count('todo'), cancelled: count('cancelled') };
for (const line of tap.split('\n')) if (/^\s*not ok /.test(line)) console.log(line);
console.log(JSON.stringify({ mode: baseline ? 'baseline' : 'record', status: r.status, signal: r.signal, ...counts }));
if (out) {
  const files = fs.readdirSync(out).filter(n => /^\d{3}-.*\.json$/.test(n));
  let bytes = 0, replayable = 0;
  for (const n of files) {
    const text = fs.readFileSync(path.join(out, n), 'utf8');
    bytes += Buffer.byteLength(text);
    if (JSON.parse(text).replayable) replayable++;
  }
  console.log(JSON.stringify({ traces: files.length, replayable, bytes }));
}
process.exit(r.status === 0 ? 0 : 1);
