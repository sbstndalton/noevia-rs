#!/usr/bin/env node
// Test-oracle glue only: drives noevia-core's own chat-partial.cjs (#1507, #2146) through synthetic replies: a reply
// starts (an earlier partial is superseded), its partial is saved at each tick, and it finishes (cleared, or kept
// without the running marker when the transcript did not take it). Timers and the clock are the oracle's, so every
// tick is explicit; NOEVIA_START_ID is fixed. Records each file's bytes after each step. Synthetic data only.
//   node record-partial-saves.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), os = require('node:os'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
process.env.NOEVIA_START_ID = '0123456789abcdef0123456789abcdef';
let ticks = [];
global.setInterval = (fn) => { ticks.push(fn); return { unref() {} }; };
global.clearInterval = () => {};
let clock = 1700000000000;
Date.now = () => clock;
const partial = require(path.join(core, 'server', 'chat-partial.cjs'));
let seed = 71;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const BIG = 'a'.repeat(65530) + '😀'.repeat(8) + 'z'.repeat(3);
const ENTRIES = [null,
  { role: 'assistant', content: 'Partial answer', model: 'm' },
  { role: 'assistant', content: '', model: 'stopped', reasoning: 'thinking…' },
  { role: 'assistant', content: BIG, reasoning: BIG, toolCalls: [{ name: 'web_search', status: 'stopped' }] },
  { role: 'assistant', savedAt: 5, content: 'x', paused: { reason: 'stopped', applied: 1 } },
  { content: 7, role: 'assistant' },
  { role: 'assistant', reasoning: 42 },
  { role: 'assistant', content: null, truncated: false, interrupted: false, running: { start: 'other' } }];
const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), start: process.env.NOEVIA_START_ID, cases: [] };
// A file as its SHA-256 and size (and its text when small); the big entry as a token the test expands.
const crypto = require('node:crypto');
const read = (dir, id) => {
  let bytes; try { bytes = fs.readFileSync(partial.fileFor(dir, id)); } catch { return null; }
  return { sha256: crypto.createHash('sha256').update(bytes).digest('hex'), bytes: bytes.length, ...(bytes.length < 2000 ? { text: bytes.toString('utf8') } : {}) };
};
for (let i = 0; i < 400; i++) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-partial-'));
  try {
    const id = pick(['chat-1', 'c_2', 'x'.repeat(40)]);
    const steps = [];
    // An earlier reply's partial (another start's, or this one's) is superseded by a new reply.
    const earlier = pick([null, '{"role":"assistant","content":"old","interrupted":true}', '[1]']);
    if (earlier) { fs.mkdirSync(path.dirname(partial.fileFor(dir, id)), { recursive: true }); fs.writeFileSync(partial.fileFor(dir, id), earlier); }
    ticks = [];
    let current = null;
    const saves = partial.startPartialSaves({ dir, chatId: id, record: { partialEntry: () => current } });
    steps.push({ op: 'start', file: read(dir, id) });
    const n = rnd(4);
    for (let k = 0; k < n; k++) {
      current = pick(ENTRIES); clock += 4000;
      ticks.forEach((t) => t());
      steps.push({ op: 'tick', entry: current, now: clock, file: read(dir, id) });
    }
    const keep = rnd(3) === 0 ? pick(ENTRIES.filter(Boolean)) : undefined;
    clock += 10;
    const stuck = rnd(8) === 0 && fs.existsSync(path.dirname(partial.fileFor(dir, id))); // the folder refuses the unlink: the file is rewritten without the marker
    if (stuck) fs.chmodSync(path.dirname(partial.fileFor(dir, id)), 0o500);
    const errors = []; const err = console.error; console.error = (...a) => errors.push(String(a[0]).slice(0, 40));
    saves.finish(keep ? { keep } : undefined);
    console.error = err;
    if (stuck) fs.chmodSync(path.dirname(partial.fileFor(dir, id)), 0o700);
    steps.push({ op: 'finish', keep: keep ?? null, stuck, now: clock, file: read(dir, id), errors: errors.length });
    // A tick after the end writes nothing.
    current = ENTRIES[1]; ticks.forEach((t) => t());
    steps.push({ op: 'late', file: read(dir, id) });
    out.cases.push({ id, earlier, steps });
  } finally { try { fs.chmodSync(dir, 0o700); } catch {} fs.rmSync(dir, { recursive: true, force: true }); }
}
fs.writeFileSync(process.argv[3], JSON.stringify(out, (_k, v) => (v === BIG ? '<BIG>' : v)));
console.log('cases', out.cases.length, 'stuck', out.cases.filter((c) => c.steps.at(-2).stuck).length, 'kept files', out.cases.filter((c) => c.steps.at(-2).file).length);
