'use strict';
// Records noevia-core's own answers to an auto-tune start (llamacpp-full-autotune.cjs `start`, the
// call routes/models.cjs makes for POST /api/models/autotune) over synthetic inputs, for
// tests/fixtures.rs to replay against `autotune_start::admit`. Synthetic data only: a generated
// models.ini in a temp directory, a fake router listing, no engine, no network.
//
//   node record-node.cjs <noevia-core checkout> > autotune-start.node.json
//
// The checkout needs server/wasm/dav-parse.wasm (built from this repository at the ref in
// server/dav-parse.lock): the ladder check and the scan in `start` run there. The body is turned into
// the start() arguments exactly as the route does (`String(body?.model||'')`, `untuned === true`,
// `mode` only when present). Every admitted case is made to reach 202 (one configured, untuned chat
// model), so "admitted" is Node's 202 with the job's mode and prompt time limit, never a guess.
const fs = require('node:fs'), os = require('node:os'), path = require('node:path'), crypto = require('node:crypto');
const core = path.resolve(process.argv[2] || '.');
const req = (m) => require(path.join(core, 'server', m));
const SOURCE = path.join(core, 'server', 'llamacpp-full-autotune.cjs');
const { createFullAutotuner } = req('llamacpp-full-autotune.cjs');
const { createPresetStore } = req('llamacpp-presets.cjs');
const { isSystemModel } = req('model-system.cjs');
console.warn = () => {};

const MODEL = 'qa-synthetic';
const INI = 'version = 1\n[' + MODEL + ']\nmodel = /models/qa-synthetic.gguf\nctx-size = 8192\nparallel = 1\n'
  + '[laya-qa]\nmodel = /models/laya-qa.gguf\nctx-size = 4096\n';
const identity = (model) => ({ arch: 'synthetic', quant: 'Q8_0', artifact: 'synthetic:' + model, build: 'qa-v1', profile: '0'.repeat(64), hardware: 'qa' });

async function answer(body, { runActive = false } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-autotune-start-'));
  try {
    const ini = path.join(dir, 'models.ini'), stateFile = path.join(dir, 'tune.json');
    fs.writeFileSync(ini, INI);
    if (runActive) fs.writeFileSync(stateFile, JSON.stringify({ job: { id: 'qa-job', status: 'running', models: [] } }));
    const presets = createPresetStore(ini);
    const rows = [MODEL, 'laya-qa'].map((id) => ({ id, status: { value: 'unloaded', args: ['--model', '/models/' + id + '.gguf'] } }));
    // The run that a 202 starts waits for chat to go idle; that wait never ends here, so nothing is
    // measured, loaded or written after the job record.
    const maintenance = { holdWhenIdle: () => new Promise(() => {}) };
    const tuner = createFullAutotuner({
      request: async () => ({ ok: true, body: {} }), rawModels: async () => ({ ok: true, body: { data: rows } }),
      presets, maintenance, applyUnlocked: async () => ({ ok: false, status: 500, body: {} }), identityFor: async (m) => identity(m),
      contextFactory: () => { throw Error('not used'); }, stateFile, now: () => 1700000000000, sleep: async () => {},
      readMemory: () => 20, planFacts: async () => null,
    });
    const r = await tuner.start(String(body?.model || ''), { confirmPause: body?.confirmPause, promptBudgetSeconds: body?.promptBudgetSeconds,
      untuned: body?.untuned === true, ...(body?.mode !== undefined ? { mode: body.mode } : {}) });
    return r.status === 202
      ? { status: 202, mode: r.body.mode, promptBudgetSeconds: r.body.promptBudgetSeconds, bulk: r.body.bulk }
      : { status: r.status, error: r.body.error };
  } finally { fs.rmSync(dir, { recursive: true, force: true }); }
}

const ok = { confirmPause: true, model: MODEL };
const CASES = [
  ['fast default', ok],
  ['long default', { ...ok, mode: 'long' }],
  ['fast explicit', { ...ok, mode: 'fast' }],
  ['fast minimum 15', { ...ok, promptBudgetSeconds: 15 }],
  ['fast maximum 1800', { ...ok, promptBudgetSeconds: 1800 }],
  ['long minimum 15', { ...ok, mode: 'long', promptBudgetSeconds: 15 }],
  ['null seconds uses mode default', { ...ok, promptBudgetSeconds: null }],
  ['long null seconds uses long default', { ...ok, mode: 'long', promptBudgetSeconds: null }],
  ['numeric string seconds', { ...ok, promptBudgetSeconds: '120' }],
  ['padded string seconds', { ...ok, promptBudgetSeconds: ' 120 \t\n' }],
  ['byte order mark is JS whitespace', { ...ok, promptBudgetSeconds: '﻿120' }],
  ['no-break space is JS whitespace', { ...ok, promptBudgetSeconds: ' 120　' }],
  ['next line is not JS whitespace', { ...ok, promptBudgetSeconds: '\u0085120' }],
  ['hex string seconds', { ...ok, promptBudgetSeconds: '0x78' }],
  ['upper hex string seconds', { ...ok, promptBudgetSeconds: '0X78' }],
  ['octal string seconds', { ...ok, promptBudgetSeconds: '0o170' }],
  ['binary string seconds', { ...ok, promptBudgetSeconds: '0b1111000' }],
  ['hex prefix without digits', { ...ok, promptBudgetSeconds: '0x' }],
  ['signed hex is NaN', { ...ok, promptBudgetSeconds: '-0x78' }],
  ['exponent string seconds', { ...ok, promptBudgetSeconds: '1.2e2' }],
  ['trailing dot string', { ...ok, promptBudgetSeconds: '120.' }],
  ['leading dot string', { ...ok, promptBudgetSeconds: '.5e3' }],
  ['plus sign string', { ...ok, promptBudgetSeconds: '+120' }],
  ['whole float seconds', { ...ok, promptBudgetSeconds: 120.0 }],
  ['empty string seconds', { ...ok, promptBudgetSeconds: '' }],
  ['whitespace string seconds', { ...ok, promptBudgetSeconds: '   ' }],
  ['Infinity string', { ...ok, promptBudgetSeconds: 'Infinity' }],
  ['lowercase inf string', { ...ok, promptBudgetSeconds: 'inf' }],
  ['nan string', { ...ok, promptBudgetSeconds: 'nan' }],
  ['underscore digits string', { ...ok, promptBudgetSeconds: '1_20' }],
  ['trailing junk string', { ...ok, promptBudgetSeconds: '120s' }],
  ['true seconds', { ...ok, promptBudgetSeconds: true }],
  ['false seconds', { ...ok, promptBudgetSeconds: false }],
  ['one-element array seconds', { ...ok, promptBudgetSeconds: [120] }],
  ['one-element string array seconds', { ...ok, promptBudgetSeconds: ['0x78'] }],
  ['nested one-element array seconds', { ...ok, promptBudgetSeconds: [[120]] }],
  ['empty array seconds', { ...ok, promptBudgetSeconds: [] }],
  ['two-element array seconds', { ...ok, promptBudgetSeconds: [120, 1] }],
  ['object seconds', { ...ok, promptBudgetSeconds: {} }],
  ['object with own valueOf seconds', { ...ok, promptBudgetSeconds: { valueOf: 120 } }],
  ['object with own toString seconds', { ...ok, promptBudgetSeconds: { toString: 120 } }],
  ['fractional seconds', { ...ok, promptBudgetSeconds: 120.5 }],
  ['below minimum', { ...ok, promptBudgetSeconds: 14 }],
  ['above maximum', { ...ok, promptBudgetSeconds: 1801 }],
  ['just above maximum', { ...ok, promptBudgetSeconds: 1800.0000001 }],
  ['negative zero string', { ...ok, promptBudgetSeconds: '-0' }],
  ['huge number', { ...ok, promptBudgetSeconds: 1e300 }],
  ['confirm absent', { model: MODEL }],
  ['confirm false', { ...ok, confirmPause: false }],
  ['confirm string true', { ...ok, confirmPause: 'true' }],
  ['confirm one', { ...ok, confirmPause: 1 }],
  ['confirm null', { ...ok, confirmPause: null }],
  ['confirm wins over bad mode', { model: MODEL, mode: 'slow' }],
  ['mode null', { ...ok, mode: null }],
  ['mode capitalised', { ...ok, mode: 'Fast' }],
  ['mode number', { ...ok, mode: 1 }],
  ['mode array', { ...ok, mode: ['fast'] }],
  ['mode object', { ...ok, mode: {} }],
  ['mode empty string', { ...ok, mode: '' }],
  ['mode inherited name', { ...ok, mode: 'toString' }],
  ['mode proto name', { ...ok, mode: '__proto__' }],
  ['bad mode wins over bad seconds', { ...ok, mode: 'x', promptBudgetSeconds: 1 }],
  ['long bulk', { confirmPause: true, mode: 'long', untuned: true }],
  ['long bulk wins over bad seconds', { confirmPause: true, mode: 'long', untuned: true, promptBudgetSeconds: 1 }],
  ['long with truthy non-boolean untuned is not bulk', { ...ok, mode: 'long', untuned: 'yes' }],
  ['bulk fast', { confirmPause: true, untuned: true }],
  ['bad seconds wins over active run', { ...ok, promptBudgetSeconds: 1, runActive: true }],
  ['active run', { ...ok, runActive: true }],
  ['active run wins over system model', { confirmPause: true, model: 'laya-qa', runActive: true }],
  ['system model', { confirmPause: true, model: 'laya-qa' }],
  ['system model in a bulk run is not refused here', { confirmPause: true, model: 'laya-qa', untuned: true }],
];

(async () => {
  const cases = [];
  for (const [name, input] of CASES) {
    const { runActive = false, ...body } = input;
    const model = String(body?.model || '');
    cases.push({ name, input: { ...body, ...(runActive ? { runActive } : {}), systemModel: isSystemModel(model) }, node: await answer(body, { runActive }).catch((e) => ({ throws: e?.constructor?.name || 'Error' })) });
  }
  const sha256 = crypto.createHash('sha256').update(fs.readFileSync(SOURCE)).digest('hex');
  process.stdout.write(JSON.stringify({ version: 2, source: 'noevia-core server/llamacpp-full-autotune.cjs start(), recorded by tests/fixtures/record-node.cjs', sourceSha256: sha256, cases }, null, 1) + '\n');
})().catch((e) => { process.stderr.write(String(e?.stack || e) + '\n'); process.exit(1); });
