#!/usr/bin/env node
'use strict';
// Records tests/fixtures/autotune-start.v1.json from noevia-core's own server/llamacpp-full-autotune.cjs:
// each case is sent to the real start() of createFullAutotuner, wholly synthetic.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> node crates/autotune-start/tests/generate_fixtures.cjs \
//     > crates/autotune-start/tests/fixtures/autotune-start.v1.json
//
// Stubs, installed in the require cache before the module loads: model-system.cjs (no models.ini
// is read; the case says whether the model is a system model) and llamacpp-calibration.cjs, whose
// ladder() is the first step after the admission checks. It throws a sentinel 503, so a case that
// passes every admission check ends there, before any flush, scan, hold or preset write. The engine,
// presets and maintenance dependencies are stubs that throw if reached. The state file lives in a
// temporary directory ({"job":{"status":"running"}} for the run-active cases).
// For an admitted case the prompt time limit recorded is Node's own Number(value ?? MODES[mode].defaultSeconds).

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const server = path.join(path.resolve(checkout), 'server');
const SOURCES = { 'server/llamacpp-full-autotune.cjs': 'f53b8fd7826bd78390b84d8c077411c4629fe2a070caeb540dfe7cbc0f27d5f1' };
for (const [rel, want] of Object.entries(SOURCES)) {
  const got = crypto.createHash('sha256').update(fs.readFileSync(path.join(checkout, rel))).digest('hex');
  if (got !== want) {
    process.stderr.write(`source hash mismatch for ${rel}: ${got}\n`);
    process.exit(1);
  }
}

const SYSTEM = 'qa-system-model';
const SENTINEL = 'QA admission passed';
function stub(file, exports) {
  const full = path.join(server, file);
  const m = new Module(full);
  m.filename = full;
  m.loaded = true;
  m.exports = exports;
  require.cache[full] = m;
}
const realSystem = require(path.join(server, 'model-system.cjs'));
stub('model-system.cjs', { ...realSystem, isSystemModel: (id) => id === SYSTEM, SYSTEM_MODEL_REASON: realSystem.SYSTEM_MODEL_REASON });
stub('llamacpp-calibration.cjs', {
  ladder() { throw Object.assign(new Error(SENTINEL), { code: 'autoconfig_impl', status: 503 }); },
  readMemAvailableGib: () => 64,
});
const { createFullAutotuner, MODES } = require(path.join(server, 'llamacpp-full-autotune.cjs'));

const unreachable = (name) => () => { throw new Error(`QA stub ${name} reached`); };
const BUDGETS = [
  undefined, null, 15, 1800, 14, 1801, 120.5, 120.0, 0, -15, 1e3, 1800.0000001, 1e21,
  '120', ' 120 ', '\t120\n', ' 120', '﻿120', '\u0085120', '0x78', '0X78', '0o170', '0b1111000', '0x', '0x+78', '-0x78',
  '1.2e2', '120.', '.5e3', '+120', '-120', '120abc', 'soon', '', '   ', 'Infinity', 'inf', 'NaN',
  true, false, [], [120], ['120'], [[120]], [1, 2], [null], [true], {}, { valueOf: 120 },
];
const inputs = [];
for (const v of BUDGETS) inputs.push({ confirmPause: true, ...(v === undefined ? {} : { promptBudgetSeconds: v }) });
for (const mode of [undefined, null, 'fast', 'long', 'Fast', 'turbo', '', 1, ['fast'], 'constructor', 'toString', '__proto__', 'hasOwnProperty'])
  for (const bulk of [false, true]) inputs.push({ confirmPause: true, ...(mode === undefined ? {} : { mode }), bulk });
for (const confirmPause of [undefined, null, false, 'true', 1, [true], { }]) inputs.push({ ...(confirmPause === undefined ? {} : { confirmPause }) });
inputs.push({ confirmPause: false, mode: 'turbo' });
inputs.push({ confirmPause: true, mode: 'long', bulk: true, promptBudgetSeconds: 5 });
inputs.push({ confirmPause: true, mode: 'long', promptBudgetSeconds: 5 });
inputs.push({ confirmPause: true, mode: 'long' });
for (const runActive of [false, true]) for (const systemModel of [false, true]) for (const bulk of [false, true])
  for (const promptBudgetSeconds of [120, 5]) inputs.push({ confirmPause: true, runActive, systemModel, bulk, promptBudgetSeconds });

(async () => {
  const cases = [];
  for (const input of inputs) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-autotune-start-'));
    const stateFile = path.join(dir, 'autotune.json');
    if (input.runActive) fs.writeFileSync(stateFile, JSON.stringify({ job: { status: 'running' } }));
    const tuner = createFullAutotuner({
      request: unreachable('request'), rawModels: unreachable('rawModels'),
      presets: { get: unreachable('presets.get'), snapshot: unreachable('presets.snapshot') },
      maintenance: {}, applyUnlocked: unreachable('applyUnlocked'), identityFor: unreachable('identityFor'),
      contextFactory: unreachable('contextFactory'), stateFile,
    });
    const { runActive, systemModel, bulk, ...fields } = input;
    const body = { ...fields, ...(bulk ? { untuned: true } : {}) };
    let reply;
    try {
      reply = await tuner.start(systemModel ? SYSTEM : 'qa-chat-model', body);
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
    let expect;
    if (reply.status === 503 && reply.body?.error === `Auto-tune is unavailable: ${SENTINEL}`) {
      const mode = fields.mode === undefined ? 'fast' : fields.mode;
      expect = { ok: { mode, promptBudgetSeconds: Number(body.promptBudgetSeconds ?? MODES[mode].defaultSeconds) } };
    } else {
      expect = { refuse: { status: reply.status, message: reply.body?.error } };
    }
    // `bulk` is sent as the body's `untuned` (the route passes it through).
    cases.push({ input: { ...fields, bulk: !!bulk, runActive: !!runActive, systemModel: !!systemModel }, expect });
  }
  process.stdout.write(JSON.stringify({
    version: 1,
    source: 'Recorded by tests/generate_fixtures.cjs from noevia-core server/llamacpp-full-autotune.cjs start() (sha256 in sources).',
    sources: SOURCES,
    cases,
  }, null, 1) + '\n');
})().catch((err) => { process.stderr.write(String(err?.stack || err) + '\n'); process.exit(1); });
