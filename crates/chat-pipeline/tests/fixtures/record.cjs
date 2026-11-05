#!/usr/bin/env node
// Test-oracle glue only: asks noevia-core's own chat.cjs (normalizeReplayHistory, and the history mapping and system
// notes of createChatHandler, read from chat.cjs itself), tools-mode.cjs and chat-sources.cjs on synthetic histories,
// and writes their answers for tests/differential.rs. Synthetic data only.
//   node record.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const req = (m) => require(path.join(core, 'server', m));
const chat = req('chat.cjs'), toolsMode = req('tools-mode.cjs'), sources = req('chat-sources.cjs');
const source = fs.readFileSync(path.join(core, 'server', 'chat.cjs'), 'utf8');
// The history mapping lives inside createChatHandler: its own lines, run as they stand.
const from = source.indexOf('    const appliedEntry = (h) =>');
const end = source.indexOf(': { role: h.role, content: h.content });', from);
if (from < 0 || end < 0) throw new Error('the history mapping moved in chat.cjs');
const mapping = source.slice(from, end + ': { role: h.role, content: h.content });'.length);
// eslint-disable-next-line no-new-func
const mapHistory = new Function('history', 'HISTORY_CAP', 'require', `${mapping}\nreturn mappedHistory;`);
const fnSource = (name) => {
  const at = source.indexOf(`function ${name}(`);
  if (at < 0) throw new Error(`${name} moved`);
  const stop = source.indexOf('\n}\n', at);
  return source.slice(at, stop + 2);
};
// eslint-disable-next-line no-new-func
const notes = new Function(`${fnSource('withSystemNote')}\n${fnSource('withoutSystemNote')}\nreturn { withSystemNote, withoutSystemNote };`)();
const near = (p) => require(path.join(core, 'server', p));
const rq = (m) => (m.startsWith('./') ? near(m) : require(m));

// A small deterministic generator (no Math.random: the fixtures are reproducible).
let seed = 20261105;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const TEXTS = ['', 'hi', 'q?', 'a reply', 'x'.repeat(520), '😀'.repeat(260), 'line\nbreak', '</untrusted> sneaky', '\ud83d', 'é'.repeat(301), '  ', 7, null, ['a', 'b'], { k: 1 }];
const NAMES = ['web_search', 'tavily_search', 'read_project_file', 'write_note', 'wiki_lookup', 'bad name!', 'x'.repeat(81), '', 3, undefined, 'drive_read'];
const ROLES = ['user', 'assistant', 'tool', 'function', 'system', 'other', undefined];
function entry() {
  const e = { role: pick(ROLES), content: pick(TEXTS) };
  const r = rnd(10);
  if (r < 3) e.name = pick(NAMES);
  if (r === 3) Object.assign(e, { role: 'tool', name: pick(NAMES), applied: true, ...(rnd(2) ? { target: pick(TEXTS) } : {}), ...(rnd(2) ? { args: pick(TEXTS) } : {}) });
  if (r === 4) Object.assign(e, { role: 'tool', name: pick(NAMES), declined: true, ...(rnd(3) === 0 ? { applied: true } : {}) });
  if (r === 5 && rnd(2)) e.failure = pick(['', '   ', 'The model stopped.', 'x'.repeat(1200)]);
  if (r === 6) e.sources = [{ id: 's1' }];
  return e;
}
const BOXES = [
  { id: 'web-search', tools: [{ function: { name: 'web_search' } }, { function: { name: 'tavily_search' } }] },
  { id: 'offline-wikipedia', tools: [{ function: { name: 'wiki_lookup' } }] },
  { id: 'notes', tools: [{ function: { name: 'write_note' } }, { function: { name: 'tavily_search' } }] },
  { id: 'google-drive', tools: [{ function: { name: 'drive_read' } }] },
];
const boxSets = [BOXES, BOXES.slice(0, 2), [], [BOXES[0]], null];
const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), cases: [] };
const record = (f) => { try { return { out: f() }; } catch (e) { return { throws: e.constructor.name }; } };
const fixed = [
  [{ role: 'user', content: 'a' }, { role: 'user', content: 'b' }],
  [{ role: 'assistant', content: 'stray' }, { role: 'user', content: 'q' }, { role: 'assistant', content: 'r1' }, { role: 'assistant', content: 'r2' }],
  [{ role: 'tool', name: 'write_note', applied: true, content: 'ok', target: 'notes/a.md', args: '{"t":1}' }, { role: 'user', content: 'q' }],
  [{ role: 'user', content: 'q' }, { role: 'tool', name: 'web_search', content: 'found' }, { role: 'tool', name: 'drive_read', content: 'secret' }, { role: 'tool', name: 'tavily_search', content: 'more' }],
  [{ role: 'user', content: 'q' }, { role: 'assistant', content: 'r' }, { role: 'tool', name: 'drive_read', content: 'secret' }, { role: 'tool', name: 'write_note', declined: true, content: 'No change' }],
  [{ role: 'user', content: { toString: 1 } }, { role: 'user', content: 'x' }],
  [null], [7, 'str', ['arr']],
];
const histories = [...fixed];
for (let i = 0; i < 400; i++) histories.push(Array.from({ length: rnd(9) }, entry));
for (const h of histories) {
  for (const offBox of [false, true]) {
    const boxes = pick(boxSets);
    const options = { offBox, carryOrphanTool: (n) => toolsMode.isPublicToolName(n, boxes),
      knownTool: (n) => typeof n === 'string' && (toolsMode.PRIVATE_CORE_TOOLS.has(n) || (boxes || []).some((b) => (b?.tools || []).some((t) => t?.function?.name === n))) };
    const message = pick(['next', undefined, '', 5]);
    out.cases.push({ history: h, message: message === undefined ? null : message, messageUndefined: message === undefined, offBox, boxes,
      normalized: record(() => chat.normalizeReplayHistory(h, message, options)) });
  }
  const cap = pick([0, 3, 50, 1]);
  out.cases.push({ history: h, cap, mapped: record(() => mapHistory(h, cap, rq)) });
}
out.public = NAMES.flatMap((n) => boxSets.map((b) => ({ name: n === undefined ? null : n, boxes: b, out: toolsMode.isPublicToolName(n, b) })));
out.failed = [...histories.flat(), { role: 'assistant', failure: ' x' }, { role: 'assistant', failure: '﻿' }].map((e) => ({ e: e === undefined ? null : e, out: sources.isFailedTurn(e) }));
const msgSets = [[], [{ role: 'user', content: 'q' }], [{ role: 'system', content: 'S' }, { role: 'user', content: 'q' }], [{ role: 'system', content: 7 }, { role: 'system', content: 'S2' }], [null, { role: 'system', content: 'a\n\nNOTE b\n\nNOTE' }]];
out.notes = msgSets.flatMap((m) => ['NOTE', '', chat.CODE_ROUTE_NOTE].map((n) => ({ m, n, with: record(() => notes.withSystemNote(m, n)), without: record(() => notes.withoutSystemNote(m, n)) })));
fs.writeFileSync(process.argv[3], JSON.stringify(out));
console.log(`recorded ${out.cases.length} cases at core ${out.core}`);
