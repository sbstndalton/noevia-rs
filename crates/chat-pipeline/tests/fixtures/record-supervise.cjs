#!/usr/bin/env node
// Test-oracle glue only: calls noevia-core's own step-supervision.cjs (createStepSupervision().decide and
// superviseNextStep) and vision.cjs (createVisionProbe) with synthetic decision providers, deadlines, abort signals,
// fetch answers and a fake clock, and records each answer, log line, provider input and request. Synthetic only.
// Needs dav-parse.wasm (decision causeOf): DAV_PARSE_WASM.
//   node record-supervise.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const S = (m) => require(path.join(core, 'server', m));
const { createStepSupervision, superviseNextStep } = S('step-supervision.cjs');
const { createVisionProbe } = S('vision.cjs');
let seed = 91;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const MSG = [{ role: 'user', content: 'Find my synthetic notes' }, { role: 'assistant', content: 'Looking.' }, { role: 'tool', content: 'ok: three notes' },
  { role: 'tool', content: 'ERROR: not found' }, { role: 'tool', content: 'error lower' }, { role: 'tool', content: 'x'.repeat(2000) }, { role: 'assistant', content: null },
  { role: 'user', content: 'and the second one?' }, { role: 'system', content: 'sys' }, { role: 'tool', content: ' ERROR later' }];
const RESULTS = [{ action: 'continue' }, { action: 'verify' }, { action: 'escalate', scores: { continue: 0.1, verify: 0.2, escalate: 0.9 } },
  { action: 'escalate', scores: { continue: 0.4, verify: 0.2, escalate: 0.65 } }, { action: 'escalate' }, { action: 'escalate', scores: { continue: 0.1, verify: 0.1, escalate: 0.6 } },
  { action: 'stop' }, { action: 'verify', extra: 1 }, null, 'continue', 5, [], { action: 'verify', scores: null }];
(async () => {
  const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), supervise: [], vision: [] };
  for (let i = 0; i < 1500; i++) {
    const n = rnd(12), messages = []; for (let k = 0; k < n; k++) messages.push(pick(MSG));
    const sc = { messages, round: pick([0, 0, 1, 2, 3]), enabled: rnd(8) > 0, preAborted: rnd(12) === 0, abortDuring: rnd(14) === 0, deadline: pick([5, 5, 500, 0, 2001, 1.5, 'x', 2000, 1]),
      behaviour: pick(['result', 'result', 'result', 'slow', 'throw']), result: pick(RESULTS), wrote: rnd(3) === 0, goal: pick([undefined, undefined, 'Explicit goal', 'g'.repeat(1600), 7]),
      next: rnd(3) === 0 };
    const logs = [], inputs = [];
    const controller = new AbortController();
    if (sc.preAborted) controller.abort();
    const provider = { decide: async (input, { signal }) => {
      inputs.push(JSON.parse(JSON.stringify(input)));
      if (sc.abortDuring) setTimeout(() => controller.abort(), 1);
      if (sc.behaviour === 'throw') throw Object.assign(new Error('synthetic'), { code: 'ECONNREFUSED' });
      if (sc.behaviour === 'slow') await new Promise((r) => { const t = setTimeout(r, 60); signal.addEventListener('abort', () => { clearTimeout(t); r(); }); });
      if (sc.abortDuring) await new Promise((r) => setTimeout(r, 5));
      return sc.result;
    } };
    const sup = createStepSupervision({ enabled: () => sc.enabled, provider, getDeadlineMs: () => sc.deadline, log: (e) => logs.push(e) });
    let decision, step = null;
    if (sc.next) { step = await superviseNextStep(sup, { round: sc.round, messages: sc.messages, signal: controller.signal, goal: sc.goal, wrote: sc.wrote }); decision = step.decision; }
    else decision = await sup.decide({ round: sc.round, messages: sc.messages, signal: controller.signal, goal: sc.goal, wrote: sc.wrote });
    out.supervise.push({ ...sc, aborted: controller.signal.aborted, inputs, logs, decision, step: step && { messages: step.messages, pause: step.pause } });
  }
  for (let i = 0; i < 300; i++) {
    let clock = 1000;
    const calls = [], answers = [];
    const behaviour = [];
    const fetchImpl = async (url, init) => { const b = pick(['ok', 'ok', 'mmproj', 'Projector', 'other', 'throw', '204']); behaviour.push(b); calls.push({ url, body: init.body, headers: init.headers });
      if (b === 'throw') throw new Error('net');
      const status = b === 'ok' ? 200 : b === '204' ? 204 : 400; const text = b === 'mmproj' ? 'missing mmproj file' : b === 'Projector' ? 'No PROJECTOR loaded' : 'bad';
      return new Response(status === 204 ? null : text, { status }); };
    const admitMode = pick([null, null, 'ok', 'throw', 'throwBare']);
    const admits = [];
    const admit = admitMode && (async () => { admits.push(admitMode); if (admitMode === 'throw') throw Object.assign(new Error('x'), { publicMessage: 'Loading is paused.' }); if (admitMode === 'throwBare') throw new Error('y'); });
    const probe = createVisionProbe({ fetchImpl, now: () => clock, admit });
    const steps = [];
    for (let k = 0; k < 1 + rnd(6); k++) {
      clock += pick([0, 10000, 31000, 301000]);
      const args = [pick(['http://engine.invalid/v1', 'http://engine.invalid//', 'http://other.invalid']), pick([{}, { Authorization: 'Bearer synthetic' }]), pick(['m-a', 'm-b'])];
      const before = calls.length;
      const result = await probe(...args);
      steps.push({ now: clock, args, result, fetched: calls.slice(before), status: behaviour.slice(before) });
    }
    out.vision.push({ admit: admitMode, steps });
  }
  fs.writeFileSync(process.argv[3], JSON.stringify(out));
  const k = {}; for (const c of out.supervise) { const x = `${c.decision?.action}/${c.logs[0]?.fellBack ?? '-'}/${c.logs[0]?.cause ?? '-'}`; k[x] = (k[x] || 0) + 1; }
  console.log(JSON.stringify(k));
})();
