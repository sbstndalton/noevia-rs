#!/usr/bin/env node
'use strict';
// R6 oracle recorder driver: runs noevia-core's llamacpp-full-autotune.test.cjs, then
// scenarios.test.cjs (beside this file), each in-process under trace-preload.cjs, and writes one trace
// per createFullAutotuner instance into <out>: NNN-*.json from core's tests, SNNN-*.json from the
// scenarios.
//   node record-node.cjs <core checkout> <out dir>             record (replaces <out>/NNN-*.json, SNNN-*.json)
//   node record-node.cjs <core checkout> --baseline            same test files, no preload, no traces
// Prints the node:test pass/fail/skip counts per file; exits non-zero when either run fails.
const fs = require('node:fs'), path = require('node:path'), { spawnSync } = require('node:child_process');

const [coreArg, outArg] = process.argv.slice(2);
if (!coreArg || !outArg) { console.error('usage: record-node.cjs <core checkout> <out dir | --baseline>'); process.exit(2); }
const core = path.resolve(coreArg);
const testFile = path.join(core, 'server', 'llamacpp-full-autotune.test.cjs');
const scenarioFile = path.join(__dirname, 'scenarios.test.cjs');
if (!fs.existsSync(testFile)) { console.error('no test file at ' + testFile); process.exit(2); }
const baseline = outArg === '--baseline';
const out = baseline ? null : path.resolve(outArg);
const preload = path.join(__dirname, 'trace-preload.cjs');
const TRACE_NAME = /^S?\d{3}-.*\.json$/;

if (out) {
  fs.mkdirSync(out, { recursive: true });
  for (const name of fs.readdirSync(out)) if (TRACE_NAME.test(name)) fs.rmSync(path.join(out, name));
}
const env = { ...process.env, TZ: 'UTC', NODE_TEST_CONTEXT: undefined };
delete env.NODE_TEST_CONTEXT;
delete env.NOEVIA_AUTOTUNE_ALLOW_BELOW_Q5_KV;
delete env.TRACE_PREFIX;
env.TRACE_CORE = core;
if (out) env.TRACE_OUT = out;

// One test file in its own process. The scenarios' traces are prefixed `S`; each scenario has its own
// node:test timeout, and --test-force-exit ends the process once every scenario has settled.
function runFile(file, label, extra = {}, flags = []) {
  const args = [...(baseline ? [] : ['--require', preload]), ...flags, '--test-reporter=tap', file];
  const r = spawnSync(process.execPath, args, { cwd: core, env: { ...env, ...extra }, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024, timeout: 1500000 });
  const tap = (r.stdout || '') + (r.stderr || '');
  if (process.env.TRACE_TAP) fs.writeFileSync(process.env.TRACE_TAP + '.' + label, tap);
  const count = key => Number((new RegExp('^# ' + key + ' (\\d+)$', 'm').exec(tap) || [])[1] ?? NaN);
  const counts = { tests: count('tests'), pass: count('pass'), fail: count('fail'), skipped: count('skipped'), todo: count('todo'), cancelled: count('cancelled') };
  for (const line of tap.split('\n')) if (/^\s*not ok /.test(line)) console.log(label + ': ' + line);
  console.log(JSON.stringify({ file: label, mode: baseline ? 'baseline' : 'record', status: r.status, signal: r.signal, ...counts }));
  return r.status === 0;
}
const okCore = runFile(testFile, 'core');
const okScenarios = runFile(scenarioFile, 'scenarios', out ? { TRACE_PREFIX: 'S' } : {}, ['--test-force-exit']);
if (out) {
  const files = fs.readdirSync(out).filter(n => TRACE_NAME.test(n));
  for (const [label, group] of [['core', files.filter(n => !n.startsWith('S'))], ['scenarios', files.filter(n => n.startsWith('S'))]]) {
    let bytes = 0, replayable = 0;
    for (const n of group) {
      const text = fs.readFileSync(path.join(out, n), 'utf8');
      bytes += Buffer.byteLength(text);
      if (JSON.parse(text).replayable) replayable++;
    }
    console.log(JSON.stringify({ file: label, traces: group.length, replayable, bytes }));
  }
}
process.exit(okCore && okScenarios ? 0 : 1);
