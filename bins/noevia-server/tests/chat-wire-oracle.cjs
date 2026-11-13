'use strict';
// Test-only Node oracle for the front's chat event stream (chat port S7). Every case is one turn's events in contract
// §1.3's order, written twice by core's own server/rust-chat-wire.cjs at the checkout given (CI: contracts/http/
// front-core.ref): on the event stream exactly as chat.cjs writes it to everyone but the front (createWire 'sse':
// its headers, `data: <json>\n\n` per event, a relayed frame as it came, index.cjs's failure frame), and as the NDJSON
// the front's relay gets. tests/chat_turn.rs serves the NDJSON from a stand-in Core and requires the front's event
// stream to be the recorded one byte for byte (keep-alive comments aside, which the timing tests pin).
//
//   node bins/noevia-server/tests/chat-wire-oracle.cjs <core-checkout> <out.json>
//
// `end` is how Core's answer ends: `end` (res.end()), `failed` (index.cjs: a handler that throws once the headers are
// out) or `break` (Core goes away inside its next event; the front's own failure frame then ends the stream, and the
// case records Core's failure frame as `failed`). Deterministic, synthetic data only; no model runs.
const fs = require('node:fs');
const path = require('node:path');
const { EventEmitter } = require('node:events');

const [core, out] = process.argv.slice(2);
if (!core || !out) throw new Error('Usage: node chat-wire-oracle.cjs <core-checkout> <out.json>');
const wire = require(path.join(path.resolve(core), 'server', 'rust-chat-wire.cjs'));

const MODEL = 'synthetic-chat-model';
const meta = { type: 'meta', model: MODEL, chatId: 'synthetic-chat-1' };
const waiting = { type: 'telemetry', phase: 'waiting', model: MODEL };
const preparing = { type: 'status', id: 'preparing', text: 'Preparing response…' };
const context = { type: 'context', model: MODEL, limit: 32768, used: 311, free: 19349, historyCount: 1, compactedAt: null };
const generating = { type: 'status', id: 'generating', text: 'Generating response…' };
const streaming = { type: 'telemetry', phase: 'streaming', model: MODEL, timeToFirstToken: 812 };
const usage = { type: 'usage', phase: 'streaming', model: MODEL, promptTokens: 311, completionTokens: 9, totalTokens: 320, tokensPerSecond: 41.5, timeToFirstToken: 812, drafted: 0 };
const complete = { type: 'telemetry', phase: 'complete', timeToFirstToken: 812 };
const done = { type: 'done', model: MODEL };

const CASES = [
  { name: 'plain-turn', end: 'end', events: [
    meta, { type: 'sources', sources: [{ name: 'notes.md', kind: 'file' }] }, waiting, preparing, context, generating,
    { type: 'tools_scope', text: '' }, streaming, { type: 'reasoning', text: 'Thinking about tea.' },
    { type: 'delta', text: 'Steam rises' }, { type: 'delta', text: ' slowly.' }, usage, complete, done,
    { type: 'title', chatId: 'synthetic-chat-1', title: 'Tea poem' },
  ] },
  { name: 'tool-round', end: 'end', events: [
    meta, waiting, preparing, { type: 'warning', text: 'Some sources are still being read.' }, context, generating,
    { type: 'tools_scope', text: 'Files' }, streaming, { type: 'preamble', text: 'I will save it.' },
    { type: 'tool', index: 0, name: 'write_file', args: { path: 'poem.txt', content: 'Steam rises slowly.' } },
    { type: 'tool_pending', id: 'ap-00000000-0000-4000-8000-000000000001', index: 0, name: 'write_file', args: { path: 'poem.txt', content: 'Steam rises slowly.' }, target: 'poem.txt', targetKind: 'file' },
    { type: 'tool_result', index: 0, name: 'write_file', text: 'Saved poem.txt', applied: true, target: 'poem.txt' },
    streaming, { type: 'delta', text: 'Saved.' }, usage, { type: 'warning', text: 'The answer was cut at the output limit.' },
    { type: 'sources', sources: [] }, { type: 'delta', text: '' }, complete, done,
  ] },
  { name: 'paused-declined', end: 'end', events: [
    meta, waiting, preparing, context, generating, streaming,
    { type: 'tool', index: 0, name: 'delete_file', args: { path: 'old.txt' } },
    { type: 'tool_pending', id: 'ap-00000000-0000-4000-8000-000000000002', index: 0, name: 'delete_file', args: { path: 'old.txt' } },
    { type: 'tool_result', index: 0, name: 'delete_file', text: 'ERROR: the user declined this action.', declined: true },
    { type: 'paused', reason: 'declined', applied: [], declined: ['delete_file'], text: 'Paused: you declined delete_file.' },
    complete, done,
  ] },
  { name: 'early-route-question', end: 'end', events: [
    { type: 'route_pending', id: '00000000-0000-4000-8000-000000000003', flag: 'health' },
    { type: 'route_remembered', forceLocal: true, allowCloud: false },
    { ...meta, route: 'smart', routing: { route: 'local', reason: 'asked' } }, waiting, preparing, context, generating,
    streaming, { type: 'delta', text: 'Kept local.' }, usage, complete, done,
  ] },
  { name: 'compact-only', end: 'end', events: [meta, waiting, preparing, context, done] },
  { name: 'error-after-headers', end: 'end', events: [
    meta, waiting, preparing, context, generating,
    { type: 'error', code: 'machineUnreachable', text: 'The inference machine is not answering.', retryable: true },
  ] },
  { name: 'handler-throws-after-headers', end: 'failed', events: [meta, waiting, { type: 'delta', text: 'Half an ans' }] },
  { name: 'core-goes-away', end: 'break', events: [meta, waiting, streaming, { type: 'delta', text: 'Steam' }] },
  { name: 'text-edge-cases', end: 'end', events: [
    meta,
    { type: 'delta', text: 'line\nbreak\r\n\n tab\t "quote" \\back\\ \u0000 \u001f \u007f' },
    { type: 'delta', text: '   separators, 😀 pair, \ud800 lone high, \udfff lone low, é, 中文, ﻿' },
    { type: 'delta', text: 'data: not a frame\n\n: not a comment' },
    { type: 'tool', index: 1, name: 'n', args: { deep: [[[{ a: -0, b: 1e21, c: 1.5e-7, d: 2 ** 53, e: null, f: true }]]] } },
    complete, done,
  ] },
  { name: 'diary-relayed-frames', end: 'end', events: [
    { type: 'status', text: 'Connecting to diary companion…' },
    { raw: 'data: {"type":"status", "text":"Opening diary context…"}\n\n' },
    { raw: 'id: 7\nevent: message\ndata: {"type":"answer","text":"Dear diary"}\n\n' },
    { raw: 'data: {"type":"diary","saved":true}\n\n' },
    { type: 'done' },
  ] },
];

function capture() {
  const res = new EventEmitter();
  const parts = [];
  let head = null;
  Object.assign(res, {
    destroyed: false,
    writeHead(status, headers) { head = { status, headers: { ...headers } }; },
    write(s) { parts.push(String(s)); return true; },
    end(s) { if (s !== undefined) parts.push(String(s)); this.emit('finish'); },
  });
  return { res, text: () => parts.join(''), head: () => head };
}

function written(kind, c) {
  const { res, text, head } = capture();
  const w = wire.createWire(res, kind);
  w.open();
  for (const e of c.events) res.write(e.raw !== undefined ? w.raw(e.raw) : w.frame(e));
  if (c.end === 'end') res.end();
  else if (c.end === 'failed') res.end(wire.failedFrame(kind));
  else res.emit('close');
  return { head: head(), text: text() };
}

// What a reader of the event stream gets (noevia-web src/api.ts streamChat: each frame's `data: ` line, parsed), each
// as JSON.stringify writes it. The fixture keeps events only as JSON text: a lone surrogate stays escaped.
function readerEvents(stream) {
  return stream.split('\n\n').slice(0, -1).map((f) => f.split('\n').find((l) => l.startsWith('data: ')))
    .filter(Boolean).map((l) => JSON.stringify(JSON.parse(l.slice(6))));
}

const cases = CASES.map((c) => {
  const sse = written('sse', c);
  const nd = written('ndjson', c);
  // What the client reads: Core's stream, and after a break the front's failure frame.
  const client = c.end === 'break' ? sse.text + wire.failedFrame('sse') : sse.text;
  return {
    name: c.name, end: c.end,
    types: c.events.map((e) => (e.raw !== undefined ? 'raw' : e.type)),
    sseStatus: sse.head.status, sseHeaders: sse.head.headers, ndjsonHeaders: nd.head.headers,
    sse: sse.text, ndjson: nd.text, failed: wire.failedFrame('sse'), client: readerEvents(client),
  };
});
const doc = { v: 1, keepAlive: wire.KEEP_ALIVE, heartbeatMs: wire.HEARTBEAT_MS, wireHeader: wire.WIRE_HEADER, wire: wire.WIRE, ndjsonType: wire.NDJSON_TYPE, cases };
fs.writeFileSync(out, `${JSON.stringify(doc, null, 1)}\n`);
