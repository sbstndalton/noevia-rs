#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own chat.cjs markFirstOutput/reportRoundUsage (their lines, read from
// chat.cjs) over synthetic rounds with a fake clock, recording every event and hand-off. Synthetic data only.
//   node record-usage.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const source = fs.readFileSync(path.join(core, 'server', 'chat.cjs'), 'utf8');
const a = source.indexOf('    let roundStartedAt = 0;');
const b = source.indexOf('    // Track final-answer content separately for each round.', a);
if (a < 0 || b < 0) throw new Error('the usage block moved in chat.cjs');
// eslint-disable-next-line no-new-func
const make = new Function('S', `
  const Date = { now: () => S.clock };
  const { send, recordUsage, chatWorkspace, model, provider, DEFAULT_PROVIDER_ID, modelManager, prefill, exchangeStartedAt } = S;
  ${source.slice(a, b)}
  return { markFirstOutput, reportRoundUsage, round() { roundStartedAt = Date.now(); roundFirstTokenMs = null; } };`);
let seed = 31;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const V = [0, 1, 5, 12, 128, -1, 1.5, '7', ' 8 ', '', null, undefined, '0x10', true, [3], {}, 2 ** 53, 'abc'];
const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), cases: [] };
for (let i = 0; i < 800; i++) {
  const log = [];
  const S = { clock: 1000, send: (e) => log.push(['send', e]), recordUsage: (_w, m, r) => log.push(['record', m, r]), chatWorkspace: {}, model: 'm-synthetic',
    provider: { id: pick(['local', 'cloud']) }, DEFAULT_PROVIDER_ID: 'local', modelManager: { recordEvidence: (m, ev) => { log.push(['evidence', m, ev.value]); return Promise.resolve(); } },
    prefill: { recordSample: (m, p, ms) => log.push(['prefill', m, p, ms]) }, exchangeStartedAt: 1000 - rnd(50) };
  const fns = make(S);
  const ops = [];
  for (let k = 3 + rnd(10); k > 0; k--) {
    S.clock += rnd(400);
    const kind = pick(['round', 'first', 'first', 'usage', 'usage']);
    if (kind === 'round') { fns.round(); ops.push(['round', S.clock]); }
    else if (kind === 'first') { fns.markFirstOutput(); ops.push(['first', S.clock]); }
    else {
      const raw = pick([null, 5, 'x', { prompt_tokens: pick(V), completion_tokens: pick(V), total_tokens: pick(V) }, { prompt_tokens: 10, completion_tokens: 4 }]);
      const timings = pick([null, undefined, { predicted_per_second: pick(V) }, { predicted_per_second: 20, draft_n: pick(V), draft_n_accepted: pick(V) }, { draft_n: 8, draft_n_accepted: 6 }]);
      const enc = JSON.parse(JSON.stringify({ raw: raw === undefined ? null : raw, timings: timings === undefined ? null : timings }));
      fns.reportRoundUsage(raw, timings);
      ops.push(['usage', S.clock, enc.raw, enc.timings]);
    }
    log.push(['op', ops.length - 1]);
  }
  out.cases.push({ local: S.provider.id === 'local', started: S.exchangeStartedAt, ops, log: JSON.parse(JSON.stringify(log)) });
}
fs.writeFileSync(process.argv[3], JSON.stringify(out));
console.log(`recorded ${out.cases.length}`);
