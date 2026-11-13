'use strict';
// Records the pure helpers noevia-core's auto-tune runner exports (llamacpp-full-autotune.cjs:
// qualityCheck, qualityFailure, newModel, kvCandidates, preferKv, bf16Unsupported) over synthetic
// inputs, for tests/pure.rs. No engine, no files: qualityCheck gets a scripted chat.
//
//   node record-node.cjs <noevia-core checkout> > pure.node.json
const fs = require('node:fs'), path = require('node:path'), crypto = require('node:crypto');
const core = path.resolve(process.argv[2] || '.');
const SOURCE = path.join(core, 'server', 'llamacpp-full-autotune.cjs');
const m = require(SOURCE);
const KV_ENV = 'NOEVIA_AUTOTUNE_ALLOW_BELOW_Q5_KV';

// One reply per probe in QUALITY order; a reply is what the runner's chat() returns.
const R = (text, extra = {}) => ({ text, ...extra });
const SCRIPTS = [
  ['all right', [R('59'), R('AX-417'), R('no')]],
  ['formatting accepted', [R('**59**'), R('`AX-417`'), R('No.')]],
  ['worked sum', [R('68 - 9 = 59'), R('ax-417'), R('NO!')]],
  ['worked sum with unicode operators', [R('(17 × 4) − 9 = 68 − 9 = 59'), R(' AX-417 '), R('no')]],
  ['prose around the right answer', [R('The answer is 59'), R('code AX-417'), R('no, never')]],
  ['list of answers', [R('59 or 61'), R('AX-417, AX-418'), R('yes')]],
  ['truncated', [R('59', { finishReason: 'length' }), R('AX-417'), R('no')]],
  ['empty and missing text', [R(''), { gen: 1 }, R('   ')]],
  ['engine failures', [{ failure: 'HTTP 500' }, { failure: 'HTTP error' }, { failure: 'no response' }]],
  ['long wrong answer snippet', [R('This answer goes on and on and on, far past the snippet limit'), R('x\ny\tz'), R('🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂')]],
  ['control characters in a wrong answer', [R('a\u0000b c d\u0085e'), R('AX-417'), R('no')]],
  ['bold only on one side', [R('**59'), R('AX-417__'), R('`no`')]],
  ['nested formatting', [R('**`59`**'), R('__AX-417__'), R('`` no ``')]],
  ['trailing punctuation twice', [R('59..'), R('AX-417!'), R('no!.')]],
  ['worked sum with equals spacing', [R('17*4-9 =59'), R('=AX-417'), R('no')]],
  ['worked sum wrong value', [R('68 - 9 = 58'), R('AX-417'), R('no')]],
  ['worked sum with letters', [R('sixty-eight minus 9 = 59'), R('AX-417'), R('no')]],
  ['non-breaking spaces', [R(' 59 '), R('﻿AX-417'), R('\u0085no')]],
];
const BASELINES = [null,
  { reference: 'bf16', probes: ['arithmetic', 'reasoning'], skipped: [{ id: 'extraction', reason: 'mismatch', answer: 'AX-41' }] },
  { reference: 'f16', probes: [], skipped: [] }];

(async () => {
  const quality = [];
  for (const [name, replies] of SCRIPTS) for (const [b, baseline] of BASELINES.entries()) {
    const asked = [];
    const chat = async (model, prompt, max) => { const i = m.QUALITY.findIndex(q => q.prompt === prompt); asked.push({ prompt, max }); return replies[i]; };
    for (const model of ['qa-synthetic', 'qa-gpt-oss-20b']) {
      asked.length = 0;
      const result = await m.qualityCheck(model, chat, baseline);
      quality.push({ name: name + ' / baseline ' + b + ' / ' + model, model, replies, baseline, asked: [...asked], result, failure: m.qualityFailure(result) });
    }
  }
  const kv = [];
  for (const env of [undefined, '1', 'TRUE', 'yes', 'On', '0', 'no', ' 1']) for (const allowQ5Kv of [undefined, false, true, 'true', 1]) {
    if (env === undefined) delete process.env[KV_ENV]; else process.env[KV_ENV] = env;
    kv.push({ env: env ?? null, allowQ5Kv: allowQ5Kv ?? null, result: m.kvCandidates(allowQ5Kv === undefined ? {} : { allowQ5Kv }) });
  }
  delete process.env[KV_ENV];
  const prefer = [];
  const P = [[[], null], [['bf16'], null], [['bf16', 'q8_0'], null], [['bf16', 'q8_0'], { bf16: 8192, q8_0: 16384 }],
    [['bf16', 'q8_0'], { bf16: 8192, q8_0: 16383 }], [['bf16', 'q8_0', 'q5_1'], { bf16: 4096, q8_0: 8192, q5_1: 16384 }],
    [['bf16', 'q8_0', 'q5_1'], { bf16: 4096, q8_0: 6000, q5_1: 8192 }], [['bf16', 'q8_0'], { bf16: null, q8_0: 4096 }],
    [['bf16', 'q8_0'], { q8_0: 4096 }], [['bf16', 'q8_0'], { bf16: null, q8_0: null }], [['q8_0', 'bf16'], { bf16: 65536, q8_0: 4096 }],
    [['f16', 'q8_0'], { f16: 0, q8_0: 0 }], [['bf16', 'q8_0'], {}]];
  for (const [passed, ceilings] of P) prefer.push({ passed, ceilings, result: m.preferKv(passed, ceilings) ?? null });
  const bf16 = [];
  for (const text of [null, '', 'error: unsupported cache type bf16', 'Unsupported KV cache type', 'CACHE_TYPE-K bf16 not supported',
    'cache type ' + 'x'.repeat(24) + 'not supported', 'cache type ' + 'x'.repeat(25) + 'not supported', 'cache type 🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂not supported',
    'cache type 🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂🙂not supported', 'bf16: missing op', 'no kernel for bf16 matmul', 'device supports bf16\nout of memory',
    'bf16\r\nnot supported', 'bf16 not supported', 'does not support\tbf16', 'bf16' + ' '.repeat(100) + 'unsupported', 'bf16' + 'y'.repeat(81) + 'unsupported',
    'BF16_UNSUPPORTED', 'ggml: type bf16 is not supported by this backend', 'out of memory allocating KV cache', 'İbf16 not supported']) {
    bf16.push({ text, result: m.bf16Unsupported(text) });
  }
  const models = [];
  for (const [model, planned, kvList] of [['qa-synthetic', false, ['bf16', 'q8_0']], ['qa-synthetic', true, ['bf16', 'q8_0']],
    ['qa-synthetic-long', true, ['bf16', 'q8_0', 'q5_1', 'q5_0']], ['qa-synthetic', false, ['f16']], ['qa-synthetic', true, []]]) {
    models.push({ model, planned, kv: kvList, result: m.newModel(model, planned, kvList) });
  }
  const sha256 = crypto.createHash('sha256').update(fs.readFileSync(SOURCE)).digest('hex');
  process.stdout.write(JSON.stringify({ version: 1, source: 'noevia-core server/llamacpp-full-autotune.cjs exports, recorded by tests/pure/record-node.cjs', sourceSha256: sha256,
    quality: m.QUALITY, versionConst: m.VERSION, modes: m.MODES, qualityCases: quality, kv, prefer, bf16, models }, null, 1) + '\n');
})().catch((e) => { process.stderr.write(String(e?.stack || e) + '\n'); process.exit(1); });
