#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own chat.cjs lines for a round's non-streamed fallback reply and for the
// calls before they run (id dedupe, argument cleanup, preamble, the assistant message), read from chat.cjs, over
// synthetic replies and streamed slots. Synthetic data only.
//   node record-calls.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const source = fs.readFileSync(path.join(core, 'server', 'chat.cjs'), 'utf8');
const between = (from, to) => { const a = source.indexOf(from), b = source.indexOf(to, a); if (a < 0 || b < 0) throw new Error(`moved: ${from}`); return source.slice(a, b + to.length); };
const fallback = between('          const msg = full.body?.choices?.[0]?.message;', '          sawAnything = true;');
const prepare = between('        for (const tc of toolCalls.values()) tc.args = tc.tooLarge', "function: { name: tc.name, arguments: tc.args } })) };");
const near = (m) => require(m.startsWith('./') ? path.join(core, 'server', m) : m);
// eslint-disable-next-line no-new-func
const run = (code, S) => new Function('S', 'require', `const { send, markFirstOutput, reportRoundUsage, toolCalls, toolOffset, MAX_TOOL_ARGS_UNITS, shownToolName, full } = S;
  let { sawAnything, roundReasoning, roundHasContent, shownText, roundContent } = S; { ${code} ; S.assistantMsg = typeof assistantMsg === 'undefined' ? null : assistantMsg; } Object.assign(S, { sawAnything, roundReasoning, roundHasContent, shownText, roundContent });`)(S, near);
let seed = 41;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const call = () => ({ id: pick([undefined, 'call_a', 'call_b', '', 7, 'call-0', 'call-1']), function: pick([undefined, { name: pick(['web_search', 'write_note', '', undefined]), arguments: pick(['{"query":"Search the web: cats. Cite the source"}', '{"a":1}', '', undefined]) }]) });
const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), fallback: [], prepare: [] };
const base = () => ({ sawAnything: false, roundReasoning: '', roundHasContent: false, shownText: rnd(2) === 0, roundContent: pick(['', '  ', 'Let me look. ']), toolOffset: pick([0, 2]), MAX_TOOL_ARGS_UNITS: 8 * 1024 * 1024,
  shownToolName: (n) => String(n || '').slice(0, 64), toolCalls: new Map() });
for (let i = 0; i < 400; i++) {
  const log = [];
  const S = { ...base(), send: (e) => log.push(['send', e]), markFirstOutput: () => log.push(['first']), reportRoundUsage: (u, t) => log.push(['usage', u ?? null, t ?? null]) };
  const msg = pick([undefined, null, {}, { content: pick(['', 'Hi', 5]) }, { reasoning_content: 'r', content: 'x' }, { tool_calls: Array.from({ length: rnd(4) }, call) }, { content: 'a', tool_calls: [] }]);
  const body = JSON.parse(JSON.stringify({ choices: pick([[{ message: msg ?? null }], [], 'x']), usage: pick([undefined, { prompt_tokens: 3 }]), timings: pick([undefined, { predicted_per_second: 9 }]) }));
  S.full = { body };
  const before = { shownText: S.shownText, roundContent: S.roundContent, toolOffset: S.toolOffset };
  run(fallback, S);
  out.fallback.push({ body, before, log: JSON.parse(JSON.stringify(log)), state: { roundReasoning: S.roundReasoning, roundHasContent: S.roundHasContent, shownText: S.shownText, roundContent: S.roundContent, tools: [...S.toolCalls.entries()] } });
}
for (let i = 0; i < 400; i++) {
  const log = [];
  const S = { ...base(), send: (e) => log.push(['send', e]) };
  S.roundHasContent = rnd(2) === 0;
  const slots = [];
  for (let k = rnd(6); k > 0; k--) {
    const idx = pick([0, 1, 2, 3, 5]);
    const slot = { id: pick(['call_a', 'call_b', 'call-1', 'call-2', `call-${idx}`, 'call-1-2']), idMade: rnd(3) === 0, name: pick(['web_search', 'write_note', 'x']), args: pick(['{"query":"Please search the internet for dogs?"}', '{"query":"ok query"}', '{"b":1}', 'not json']), ...(rnd(8) === 0 ? { tooLarge: true } : {}) };
    if (slot.idMade) slot.id = `call-${idx}`;
    S.toolCalls.set(idx, slot);
  }
  for (const [k, v] of S.toolCalls) slots.push([k, { ...v }]);
  const before = { roundContent: S.roundContent, roundHasContent: S.roundHasContent };
  run(prepare, S);
  out.prepare.push({ slots, before, log, assistantMsg: S.assistantMsg, roundHasContent: S.roundHasContent, after: [...S.toolCalls.entries()] });
}
fs.writeFileSync(process.argv[3], JSON.stringify(out));
console.log(`fallback ${out.fallback.length}, prepare ${out.prepare.length}`);
