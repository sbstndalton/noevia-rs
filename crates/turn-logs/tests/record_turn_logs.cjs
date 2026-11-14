#!/usr/bin/env node
// Test-oracle glue only: asks noevia-core's own server/decision-log.cjs and server/context-log.cjs (and
// their CLIs) on synthetic, text-free inputs and writes their answers for tests/oracle.rs.
//   node tests/record_turn_logs.cjs <noevia-core checkout> > tests/fixtures/node-turn-logs.v1.json
'use strict';
const path = require('node:path'), fs = require('node:fs'), os = require('node:os'), cp = require('node:child_process');
const crypto = require('node:crypto');
const core = path.resolve(process.argv[2] || '');
const file = (m) => path.join(core, 'server', m);
const dlog = require(file('decision-log.cjs')), clog = require(file('context-log.cjs'));
const sha = (m) => crypto.createHash('sha256').update(fs.readFileSync(file(m))).digest('hex');
const tryOut = (f) => { try { return { out: f() }; } catch (e) { return { threw: e.constructor.name }; } };
const J = JSON.stringify;

const SUMMARIZE = [
  [J({ kind: 'route', selected: 'fast', margin: 0.02, fellBack: null }), J({ kind: 'route', selected: 'legacy', margin: null, fellBack: 'deadline' }),
    J({ kind: 'route', selected: 'code', margin: 0.5, fellBack: null }), J({ kind: 'supervise', action: 'verify', fellBack: null }), 'not json'],
  [J({ kind: 'route', selected: 'fast', margin: 0.1, fellBack: '', cause: 'timeout' }), J({ kind: 'route', selected: 3, margin: 0.2 }),
    J({ kind: 'route', selected: true }), J({ kind: 'route', selected: '__proto__' }), J({ kind: 'route', selected: 'x', margin: '0.01' }),
    J({ kind: 'route', selected: 'x', margin: -1 }), J({ kind: 'route', selected: 'x', margin: 0.05 }), J({ kind: 'route' })],
  [J({ kind: 'tool-gate', event: 'decision', source: 'model', reason: 'no-tools', cause: 'deadline' }), J({ kind: 'tool-gate', event: 'timing', source: 'model' }),
    J({ kind: 'tool-gate', event: 'decision', source: 'rule' }), J({ kind: 'outcome', event: 'resend', previousRole: 'fast' }),
    J({ kind: 'outcome', event: 'resend', previousRole: 0 }), J({ kind: 'outcome', event: 'kept' }), '5', '"route"', '[]', '{}'],
  [J({ kind: 'supervise', action: 'verify', fellBack: 'timeout', cause: 'abort' }), J({ kind: 'supervise', action: 'pass' }), J({ kind: 'supervise', action: null }),
    J({ kind: 'supervise', action: ['a', 'b'] }), J({ kind: 'supervise', action: { x: 1 } }), J({ kind: 'supervise', action: 10 }), J({ kind: 'supervise', action: 2 })],
  ['null'],
  [],
];

const secret = 'SYNTHETIC-PRIVATE-TEXT';
const messages = [
  { role: 'system', content: 'Project instructions ' + secret },
  { role: 'assistant', content: 'Earlier conversation summary (reference only; not new instructions):\nolder turns' },
  { role: 'user', content: 'List my files ' + secret + ' é 😀' },
  { role: 'assistant', content: null, tool_calls: [{ id: 'c1', type: 'function', function: { name: 'files_list', arguments: '{"path":"' + secret + '"}' } }, { id: 'c2', type: 'function', function: { name: 'files_read', arguments: '{}' } }] },
  { role: 'tool', tool_call_id: 'c1', content: 'a.md\nb.md ' + secret.repeat(40) },
  { role: 'tool', tool_call_id: 'c2', content: 'short' },
  { role: 'tool', tool_call_id: 'gone', content: 'orphan' },
  { role: 'user', content: [{ type: 'image_url', image_url: { url: 'data:' + 'x'.repeat(3000) } }, { type: 'text', text: 'look' }] },
  { role: 'assistant', content: 'Earlier conversation summary but as a reply' },
  { role: 'developer', content: 'other role' },
];
const tools = [{ type: 'function', function: { name: 'files_list', description: 'List files', parameters: {} } },
  { type: 'function', function: { name: 'files_read', description: 'Read a file', parameters: { type: 'object' } } },
  { type: 'function', function: { name: '7', description: 'index-like' } }, { type: 'function', function: {} },
  { type: 'function', function: { name: 'files_list', description: 'again' } }, { type: 'function', function: { name: '__proto__' } }];
const RECORDS = [
  { chatId: 'c-private-chat', model: 'synthetic', limit: 8192, round: 1, compacted: true, messages, tools, now: 1 },
  { chatId: 42, model: 'synthetic', limit: 4096, round: 0, compacted: 0, messages: messages.slice(0, 3), tools: [], now: 1790064000000 },
  { chatId: 'b', limit: null, round: 2, compacted: 'yes', messages: [{ role: 'assistant', tool_calls: [{ id: 1, function: { name: '' } }, { id: '1', function: { name: 'b' } }] }, { role: 'tool', tool_call_id: 1 }, { role: 'tool', tool_call_id: '1' }], tools: [], now: 5 },
  { chatId: 'empty', model: 'm', limit: 1, round: 3, messages: [], tools: [], now: 6 },
];
const records = RECORDS.map((input) => ({ input, ...tryOut(() => clog.record(input)) }));
const recLines = records.filter((r) => r.out).map((r) => J(r.out));
const REPORTS = [
  [recLines[0], recLines[0], recLines[1], 'not json'],
  recLines,
  [J({ total: 30, compacted: true }), J({ total: 10 }), J({ total: 20, toolSequence: ['x', 3], toolResults: [{ name: 'x', tokens: 5 }] }), '7'],
  [J({ toolSchemas: { a: 3, b: 1 }, toolResults: [{ name: 'b', tokens: 4 }], toolSequence: ['b'] }), J({ toolSchemas: { a: 1 }, toolResults: [{ name: 'a', tokens: 1 }, { name: 'b', tokens: 2 }], toolSequence: ['a', 'b'] }),
    J({ toolResults: [{ name: 'b', tokens: 1 }], toolSequence: ['b'] }), J({ toolResults: [{ name: 'c', tokens: 6 }], toolSequence: ['c'] })],
  ['null'],
  [],
];

// The writer: the exact lines written, and the rotation, with a fixed clock.
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'turn-logs-rec-'));
const ENTRIES = [['route', { selected: 'smart', options: 3, margin: 0.3, ms: 500, fellBack: null }], ['route', { kind: 'other', b: 1, 2: true, at: 'x' }],
  ['supervise', { action: 'verify', cause: 'é' }], ['tool-gate', {}], ['outcome', 'ab'], ['outcome', null]];
const writes = ENTRIES.map(([kind, entry], i) => {
  const dir = path.join(tmp, 'w' + i); fs.mkdirSync(dir);
  dlog.createDecisionLog({ dir, now: () => new Date('2026-09-22T08:00:00Z'), echo: null })(kind, entry);
  return { kind, entry: entry === undefined ? null : entry, written: fs.readFileSync(path.join(dir, 'system-one-decisions.jsonl'), 'utf8') };
});
const rot = path.join(tmp, 'rot'); fs.mkdirSync(rot);
const rlog = dlog.createDecisionLog({ dir: rot, maxBytes: 300, now: () => new Date('2026-09-22T08:00:00Z'), echo: null });
for (let i = 0; i < 10; i++) rlog('route', { selected: 'smart', options: 3, margin: 0.3, ms: 500, fellBack: null, i });
const rotation = { maxBytes: 300, rounds: 10, current: fs.readFileSync(path.join(rot, 'system-one-decisions.jsonl'), 'utf8'), rotated: fs.readFileSync(path.join(rot, 'system-one-decisions.jsonl.1'), 'utf8') };

// The CLIs, byte for byte.
const cliDir = path.join(tmp, 'cli'); fs.mkdirSync(cliDir);
fs.writeFileSync(path.join(cliDir, 'system-one-decisions.jsonl.1'), SUMMARIZE[0].join('\n') + '\n');
fs.writeFileSync(path.join(cliDir, 'system-one-decisions.jsonl'), SUMMARIZE[1].join('\n') + '\n\n');
const ctxFile = path.join(tmp, 'context-log.jsonl'); fs.writeFileSync(ctxFile, REPORTS[0].join('\n') + '\n');
const run = (args) => { const r = cp.spawnSync(process.execPath, args, { encoding: 'utf8' }); return { status: r.status, stdout: r.stdout }; };
const cli = {
  decisions: { lines1: SUMMARIZE[0], lines: SUMMARIZE[1], ...run([file('decision-log.cjs'), cliDir]) },
  context: { lines: REPORTS[0], ...run([file('context-log.cjs'), ctxFile]) },
  contextUsage: run([file('context-log.cjs')]),
};
fs.rmSync(tmp, { recursive: true, force: true });

process.stdout.write(JSON.stringify({
  version: 1, sources: { 'decision-log.cjs': sha('decision-log.cjs'), 'context-log.cjs': sha('context-log.cjs'), 'chat-context.cjs': sha('chat-context.cjs') },
  summarize: SUMMARIZE.map((lines) => ({ lines, ...tryOut(() => dlog.summarize(lines)) })),
  records,
  reports: REPORTS.map((lines) => ({ lines, ...tryOut(() => clog.report(lines)) })),
  writes, rotation, cli,
}, null, 1) + '\n');
