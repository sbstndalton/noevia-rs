#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own chat.cjs pause notes and end-of-turn lines (read from chat.cjs) with
// synthetic state, recording what they send. Synthetic data only.
//   node record-end.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const source = fs.readFileSync(path.join(core, 'server', 'chat.cjs'), 'utf8');
const between = (from, to) => { const a = source.indexOf(from), b = source.indexOf(to, a); if (a < 0 || b < 0) throw new Error(`moved: ${from}`); return source.slice(a, b + to.length); };
const declined = between("        const n = appliedWrites.length, names = [...new Set(declinedWrites)].join(', ');", ': `No change was made: you declined ${names}.` });');
const supervision = between("        send({ type: 'paused', reason: 'supervision', applied: n,", ": 'Step checks paused this reply before any further steps. Nothing was changed.' });");
const sensitive = between("      send({ type: 'paused', reason: 'sensitive-tool-result',", "Switch this chat to local or resend.' });");
const ending = between('    if (!body.compactOnly && roundHasContent && injectedEntries.length) {', "timeToFirstToken: exchangeFirstTokenMs === null ? null : exchangeFirstTokenMs / 1000 });");
const near = (m) => require(m.startsWith('./') ? path.join(core, 'server', m) : m);
const finalAnswer = near('./final-answer.cjs');
// eslint-disable-next-line no-new-func
const run = (code, S) => new Function('S', 'require', `const { send, appliedWrites, declinedWrites, n, cloudHold, body, injectedEntries, placedEntries, message, readableFiles, gatheredSources, model, exchangeFirstTokenMs, finalAnswer } = S;
  let { roundHasContent, roundContent, roundReasoning, paused, finalPass, replySources } = S; { ${code} } S.replySources = replySources;`)(S, near);
let seed = 37;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), pauses: [], endings: [] };
for (const applied of [0, 1, 2, 5]) for (const names of [['write_note'], ['write_note', 'write_note', 'drive_write'], ['a', 'b', 'c']]) {
  const sent = [];
  const S = { send: (e) => sent.push(e), appliedWrites: Array(applied).fill('w'), declinedWrites: names, n: applied, cloudHold: pick(['health', { kind: 'pii' }]) };
  run(declined, S); run(supervision, S); run(sensitive, S);
  out.pauses.push({ applied, names, flag: S.cloudHold, sent });
}
const FILES = [{ name: 'notes.md', content: 'x' }, { name: 'codes.txt', content: 'x', attachment: { id: 'att-1' } }];
const ENTRIES = [{ file: 'notes.md', body: 'The total is ORCHID-42 today.', kind: 'file' }, { file: 'codes.txt', body: 'INV-2042 costs 34.95', kind: 'file' }, { file: 'notes.md', body: '[Page 2]\nsomething', kind: 'excerpt' }];
for (let i = 0; i < 400; i++) {
  const sent = [];
  const S = { send: (e) => sent.push(e), body: { compactOnly: rnd(6) === 0 }, roundHasContent: rnd(3) > 0, roundContent: pick(['', 'It is ORCHID-42.', 'INV-2042 costs 34.95 today', 'plain']), roundReasoning: pick(['', '  ', 'thinking']),
    paused: rnd(4) === 0, finalPass: rnd(3) === 0, injectedEntries: ENTRIES.filter(() => rnd(2)), placedEntries: ENTRIES.filter(() => rnd(3) === 0), message: pick(['what is it?', 'ORCHID-42?', '']),
    readableFiles: FILES, replySources: Array(rnd(3)).fill({}), gatheredSources: pick([[], ['https://a.example'], ['1', '2', '3', '4', '5', '6']]), model: 'm-synthetic', exchangeFirstTokenMs: pick([null, 0, 1234]), finalAnswer };
  const state = JSON.parse(JSON.stringify({ ...S, send: undefined, finalAnswer: undefined, replySources: S.replySources.length }));
  run(ending, S);
  out.endings.push({ state, sent, replaced: S.replySources.length !== state.replySources ? S.replySources : null });
}
fs.writeFileSync(process.argv[3], JSON.stringify(out));
console.log(`pauses ${out.pauses.length}, endings ${out.endings.length}, resent ${out.endings.filter((e) => e.replaced).length}`);
