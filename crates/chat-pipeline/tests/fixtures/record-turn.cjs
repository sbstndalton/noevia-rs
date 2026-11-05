#!/usr/bin/env node
// Test-oracle glue only: feeds noevia-core's own chat-interrupted-turn.cjs createTurnRecord synthetic event streams and
// records the entries it would keep (assistantEntry, partialEntry). Synthetic data only.
//   node record-turn.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const { createTurnRecord } = require(path.join(core, 'server', 'chat-interrupted-turn.cjs'));
let seed = 19;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const TEXT = ['', 'Hello', ' world', '  ', 'Let me check. ', 'x'.repeat(4100), '😀', '\ud83d', 7, null];
const NAMES = ['web_search', 'write_note', '', 'bad name', 3, undefined];
const IDX = [0, 1, 2, 5, -1, 1.5, '1', undefined, 4294967295, 4294967294];
function event() {
  switch (rnd(14)) {
    case 0: return { type: 'meta', model: 'm', route: pick([undefined, '', 'fast', 'smart', 7]), routingDecision: pick([undefined, null, { status: 'accepted' }]), routing: pick([undefined, { route: 'local', reason: 'policy' }, { route: 'cloud', reason: 'check-unavailable', detail: pick(['timeout', 'Bad', 'x'.repeat(41), 5]) }, { route: 'other', reason: 'x' }, { route: 'local', reason: 3 }]) };
    case 1: return { type: 'sources', sources: pick([[], [{ id: 's' }], 'no']) };
    case 2: return { type: 'reasoning', text: pick(TEXT) };
    case 3: return { type: 'preamble', text: pick(['', 'Let me check. ', ' Hello', 'not there', '  padded  ']) };
    case 4: case 5: return { type: 'delta', text: pick(TEXT) };
    case 6: return { type: 'tool', index: pick(IDX), name: pick(NAMES), args: pick(['{}', '', 5]) };
    case 7: return { type: 'tool_pending', id: 'ap-1', index: pick(IDX), name: pick(NAMES), args: '{"a":1}', target: pick([undefined, '', 'notes/a.md']), targetKind: pick([undefined, 'drive', 'drive-new', 'other']) };
    case 8: case 9: return { type: 'tool_result', index: pick(IDX), name: pick(NAMES), text: pick(TEXT), applied: pick([undefined, true, 'yes']), declined: pick([undefined, true]), notRun: pick([undefined, true]), target: pick([undefined, '', 'drive:x']) };
    case 10: return { type: 'paused', reason: pick(['declined', 'supervision', 'sensitive-tool-result', 'x']), applied: pick([0, 1, 2, -1, 1.5, undefined]), declined: pick([undefined, ['write_note', 'write_note', 'bad name', 7, 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h']]) };
    case 11: return { type: 'usage', promptTokens: pick([10, undefined, '5', Infinity]), completionTokens: 3, totalTokens: 13, tokensPerSecond: pick([4.5, null]) };
    case 12: return rnd(4) === 0 ? { type: 'error', text: 'x' } : { type: 'done' };
    default: return pick([null, 'str', [1], { type: 'title', title: 't' }, { type: 'status', id: 'generating' }]);
  }
}
const strip = (v) => JSON.parse(JSON.stringify(v === undefined ? null : v));
const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), cases: [] };
for (let i = 0; i < 1500; i++) {
  const events = Array.from({ length: rnd(14) }, () => strip(event()));
  let clock = 1000;
  const now = () => (clock += 7);
  const rec = createTurnRecord({ now });
  for (const ev of events) rec.observe(ev);
  out.cases.push({ events, assistant: rec.assistantEntry(), partial: rec.partialEntry() });
}
fs.writeFileSync(process.argv[3], JSON.stringify(out));
console.log(`recorded ${out.cases.length}; kept ${out.cases.filter((c) => c.assistant).length}, partial ${out.cases.filter((c) => c.partial).length}`);
