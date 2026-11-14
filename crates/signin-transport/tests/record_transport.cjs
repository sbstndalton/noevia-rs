'use strict';
// Records core's own transport (server/chatgpt-oauth.cjs on the core checkout in $CORE) over QA
// inputs, for tests/node_replay.rs. Usage (on the N5 builder, never on MS02):
//   CORE=/path/to/core-ref node crates/signin-transport/tests/record_transport.cjs [--check]
//   Recorded against the noevia-core commit in tests/fixtures/core-ref (check out that commit as $CORE).
// Product and vendor names in core's output are replaced by neutral words of the same length
// (so core's 300/200/80-unit cuts fall at the same places); the replay configures those words.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

const CORE = process.env.CORE;
if (!CORE) throw new Error('set CORE to a core checkout');
const SRC = path.join(CORE, 'server', 'chatgpt-oauth.cjs');
const OUT = path.join(__dirname, 'fixtures', 'node-transport.v1.json');
const NOW = 1_800_000_000_000;
Date.now = () => NOW;
crypto.randomUUID = () => '00000000-0000-4000-8000-000000000000';
const mod = require(SRC);

const SCRUB = [[/ChatGPT/g, 'Service'], [/chatgpt/g, 'service'], [/Codex/g, 'Relay'], [/codex/g, 'relay'], [/OpenAI/g, 'Vendor'], [/openai/g, 'vendor']];
const scrub = (s) => SCRUB.reduce((t, [re, w]) => t.replace(re, w), String(s));
const scrubDeep = (v) => (typeof v === 'string' ? scrub(v) : Array.isArray(v) ? v.map(scrubDeep)
  : v && typeof v === 'object' ? Object.fromEntries(Object.entries(v).map(([k, x]) => [scrub(k), scrubDeep(x)])) : v);

// ── inputs ──────────────────────────────────────────────────────────────────
const contents = ['hello', '', ['x'], [{ type: 'text', text: 'a' }, { type: 'text', text: 'b' }, { type: 'image_url', image_url: { url: 'data:image/png;base64,AA' } }],
  [{ type: 'image_url', image_url: {} }, null, 5, { type: 'text', text: 7 }], null, undefined, 42, { a: 1 }, [{ type: 'text', text: 'é😀' }]];
const messageSets = [];
for (const c of contents) {
  messageSets.push([{ role: 'system', content: c }, { role: 'user', content: c }]);
  messageSets.push([{ role: 'developer', content: c }, { role: 'assistant', content: c }, { role: 'tool', tool_call_id: 'call_1', content: c }]);
}
messageSets.push([
  { role: 'assistant', content: null, tool_calls: [{ id: 'c1', function: { name: 'look', arguments: '{"q":1}' } }, { id: 'c2', function: { name: 'look' } },
    { id: 'c3', function: { name: '', arguments: 'x' } }, { function: { name: 'n' } }, null, { id: 'c4', function: { name: 'z', arguments: '' } }] },
  { role: 'tool', content: 'r' }, { role: 'tool', tool_call_id: 9, content: 'r' }, { role: 'other', content: 'x' }, null, 'str', { content: 'no role' },
]);
messageSets.push('not an array');
const toolSets = [undefined, [], [{ type: 'function', function: { name: 'look', description: 'd', parameters: { type: 'object', properties: { q: { type: 'string' } } }, strict: true } }],
  [{ type: 'function', function: { name: 'a' } }, { type: 'function', function: { name: '' } }, { type: 'other', function: { name: 'b' } }, null,
    { type: 'function', function: { name: 'c', description: 5, parameters: [], strict: 'yes' } }]];
const choices = [undefined, 'auto', 'none', 'required', 'bogus', { type: 'function', function: { name: 'look' } }, { type: 'function' }, { type: 'x', function: { name: 'y' } }];
const efforts = [undefined, 'minimal', 'low', 'medium', 'high', 'xhigh', 'default', '', 5, 'max'];
const formats = [undefined, { type: 'json_object' }, { type: 'text' }, { type: 'json_schema', json_schema: { name: 'n'.repeat(70), schema: { type: 'object' }, strict: true, description: 'd' } },
  { type: 'json_schema', json_schema: { schema: { type: 'object' } } }, { type: 'json_schema', json_schema: { name: '', schema: [] } }, { type: 'json_schema' }, 'json'];
const models = ['m-large', '', 5, null, ['m', 'x'], 'é'.repeat(90)];

const bodies = [];
for (const messages of messageSets) bodies.push({ model: 'm-large', messages });
for (const tools of toolSets) for (const tool_choice of choices) bodies.push({ model: 'm-large', messages: [], tools, tool_choice });
for (const reasoning_effort of efforts) for (const response_format of formats) bodies.push({ model: 'm-small', messages: [], reasoning_effort, response_format });
for (const model of models) bodies.push({ model, messages: [{ role: 'user', content: 'q' }], temperature: 0.2, max_tokens: 5, stream_options: {}, seed: 1 });
const requests = bodies.map((body) => ({ body: JSON.stringify(body), out: JSON.stringify(mod.toResponsesRequest(JSON.parse(JSON.stringify(body)))) }));


// ── streams ─────────────────────────────────────────────────────────────────
const ev = (o) => `data: ${JSON.stringify(o)}\n\n`;
const streams = {
  text: [ev({ type: 'response.output_text.delta', item_id: 'm1', delta: 'Hel' }), ev({ type: 'response.output_text.delta', item_id: 'm1', delta: 'lo' }),
    ev({ type: 'response.output_item.done', item: { type: 'message', id: 'm1', content: [{ type: 'output_text', text: 'Hello' }] } }),
    ev({ type: 'response.completed', response: { output: [{ type: 'message', id: 'm1', content: [{ type: 'output_text', text: 'Hello' }] }], usage: { input_tokens: 3, output_tokens: 2, total_tokens: 5 } } })],
  onlyItems: [ev({ type: 'response.output_item.done', item: { type: 'message', id: 'm1', content: [{ type: 'output_text', text: 'A' }, { type: 'text', text: 'B' }, { type: 'refusal', text: 'C' }] } }),
    ev({ type: 'response.completed', response: { output: [{ type: 'message', id: 'm2', content: [{ type: 'output_text', text: 'D' }] }], usage: { input_tokens: 1, output_tokens: 1 } } })],
  onlyCompleted: [ev({ type: 'response.completed', response: { output: [{ type: 'message', content: [{ type: 'output_text', text: 'final' }] },
    { type: 'function_call', id: 'f1', call_id: 'call_f1', name: 'look', arguments: '{"a":1}' }, { type: 'function_call', id: 'f2', name: 'x' }, { type: 'function_call', id: 'f3' }, null] } })],
  anonymous: [ev({ type: 'response.output_text.delta', delta: 'anon' }), ev({ type: 'response.output_item.done', item: { type: 'message', id: 'm1', content: [{ type: 'output_text', text: 'anon' }] } }),
    ev({ type: 'response.completed', response: {} })],
  reasoning: [ev({ type: 'response.reasoning_summary_text.delta', delta: 'think' }), ev({ type: 'response.reasoning_text.delta', delta: ' more' }), ev({ type: 'response.reasoning_text.delta', delta: '' }),
    ev({ type: 'response.output_text.delta', item_id: 'm', delta: 'ok' }), ev({ type: 'response.completed', response: { usage: 'n/a' } })],
  tools: [ev({ type: 'response.output_item.added', item: { type: 'function_call', id: 'i1', call_id: 'call_1', name: 'look' } }),
    ev({ type: 'response.function_call_arguments.delta', item_id: 'i1', delta: '{"q":' }), ev({ type: 'response.function_call_arguments.delta', item_id: 'i1', delta: '"x"}' }),
    ev({ type: 'response.function_call_arguments.delta', item_id: 'nope', delta: 'x' }),
    ev({ type: 'response.output_item.done', item: { type: 'function_call', id: 'i1', call_id: 'call_1', name: 'look', arguments: '{"q":"x"}' } }),
    ev({ type: 'response.output_item.done', item: { type: 'function_call', id: 'i2', name: 'late', arguments: '{}' } }),
    ev({ type: 'response.output_item.added', item: { type: 'function_call', id: 'i3', name: '' } }),
    ev({ type: 'response.output_item.done', item: { type: 'function_call', id: 'i4' } }),
    ev({ type: 'response.completed', response: { output: [{ type: 'function_call', id: 'i1', name: 'look', arguments: 'dup' }], usage: { input_tokens: 'a', output_tokens: 2 } } })],
  numericIds: [ev({ type: 'response.output_item.added', item: { type: 'function_call', id: 7, name: 'a' } }), ev({ type: 'response.function_call_arguments.delta', item_id: 7, delta: '1' }),
    ev({ type: 'response.output_item.done', item: { type: 'message', id: 3, content: [{ type: 'text', text: 'n' }] } }),
    ev({ type: 'response.output_item.done', item: { type: 'message', id: 3, content: [{ type: 'text', text: 'again' }] } }),
    ev({ type: 'response.output_item.added', item: { type: 'function_call', name: 'noid' } }), ev({ type: 'response.output_item.added', item: { type: 'function_call', name: 'noid2' } }),
    ev({ type: 'response.function_call_arguments.delta', delta: 'u' }), ev({ type: 'response.completed', response: {} })],
  incompleteLength: [ev({ type: 'response.output_text.delta', item_id: 'm', delta: 'cut' }), ev({ type: 'response.incomplete', response: { incomplete_details: { reason: 'max_output_tokens' } } })],
  incompleteFilter: [ev({ type: 'response.incomplete', response: { incomplete_details: { reason: 'content_filter' }, usage: { total_tokens: 0 } } })],
  incompleteOther: [ev({ type: 'response.output_item.added', item: { type: 'function_call', id: 'x', name: 'n' } }), ev({ type: 'response.incomplete', response: { incomplete_details: { reason: 'other' } } })],
  failed: [ev({ type: 'response.output_text.delta', item_id: 'm', delta: 'part' }), ev({ type: 'response.failed', response: { error: { message: 'model overloaded' } } })],
  failedBare: [ev({ type: 'response.failed', response: {} })],
  failedNone: [ev({ type: 'response.failed' })],
  failedUsage: [ev({ type: 'response.failed', response: { error: { type: 'usage_limit_reached', plan_type: 'pro', resets_at: 1_800_003_600 } } })],
  errorKnown: [ev({ type: 'error', error: { type: 'usage_limit_reached', resets_in_seconds: 90 } })],
  errorQuota: [ev({ type: 'error', code: 'insufficient_quota', message: 'x' })],
  errorNotIncluded: [ev({ type: 'error', error: { code: 'usage_not_included' } })],
  errorPlain: [ev({ type: 'error', message: 'boom '.repeat(80) })],
  errorDetail: [ev({ type: 'error', detail: { nested: true } })],
  errorEmpty: [ev({ type: 'error' })],
  errorFarDate: [ev({ type: 'error', error: { message: 'You hit a usage limit', resets_at: '1e20' } })],
  errorPlan: [ev({ type: 'error', error: { type: 'usage_limit_reached', plan_type: 'bad plan', resets_at: [1_800_000_100] } })],
  earlyEnd: [ev({ type: 'response.output_text.delta', item_id: 'm', delta: 'half' })],
  empty: [],
  // noevia#2699 item 1: TextDecoder drops a byte-order mark at the very start only, also split over chunks.
  bom: [Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), Buffer.from(ev({ type: 'response.output_text.delta', delta: 'b' }))]), ev({ type: 'response.completed', response: {} })],
  bomSplit: [Buffer.from([0xef]), Buffer.from([0xbb]), Buffer.concat([Buffer.from([0xbf]), Buffer.from(ev({ type: 'response.output_text.delta', delta: 's' }))]), ev({ type: 'response.completed', response: {} })],
  bomLater: [ev({ type: 'response.output_text.delta', delta: 'a' }), Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), Buffer.from(ev({ type: 'response.output_text.delta', delta: 'kept' }))]),
    ev({ type: 'response.completed', response: {} })],
  doneOnly: ['data: [DONE]\n\n'],
  junk: [': comment\n\n', 'event: x\ndata: not json\n\n', 'data: null\n\n', 'data: 5\n\n', 'data:   {"type":"response.output_text.delta","delta":"sp"}\r\n\r\n',
    'data: {"type":"response.output_text.delta",\ndata: "delta":"multi"}\n\n', 'data: {"type":"response.completed","response":{}}'],
  afterFinish: [ev({ type: 'response.completed', response: {} }), ev({ type: 'response.output_text.delta', delta: 'never' })],
  crlf: ['data: {"type":"response.output_text.delta","delta":"a"}\r\n\r\ndata: {"type":"response.output_text.delta","delta":"b"}\n\r\n', 'data: {"type":"response.completed","response":{}}\n\n'],
  badUtf8: [Buffer.concat([Buffer.from('data: {"type":"response.output_text.delta","delta":"x'), Buffer.from([0xff, 0xc3]), Buffer.from('"}\n\n')]),
    Buffer.concat([Buffer.from('data: {"type":"response.completed","response":{}}\n\n'), Buffer.from([0xe2, 0x82])])],
};

async function partsOf(chunks) {
  async function* src() { for (const c of chunks) yield typeof c === 'string' ? Buffer.from(c) : c; }
  const out = [];
  try { for await (const p of mod.chatParts(src())) out.push(p); } catch (e) { out.push({ thrown: String(e && e.message) }); }
  return out;
}

// ── the full fetch path ─────────────────────────────────────────────────────
const catalogue = [{ slug: 'm-large', effort: 'medium', summary: 'detailed', summaryParam: true, levels: ['low', 'medium', 'high', 'xhigh'] },
  { slug: 'm-small', summaryParam: false }, { slug: 'm-none', summary: 'none', summaryParam: true, levels: [] }, { slug: 'm-auto', summaryParam: true, effort: '' }];
// One fake backend answer: chunks (or one text), optionally labelled as a provider message, without a
// body, failing after `failAfter` chunks with `failMessage`, or a send that rejects (`throws`).
function upstreamResponse(u) {
  if (u.throws) throw new Error(u.throws);
  const headers = { 'content-type': u.contentType || 'text/event-stream', ...(u.labelled ? { 'x-noevia-provider-message': '1' } : {}) };
  if (u.nobody) return new Response(null, { status: u.status, headers });
  const chunks = (u.chunks || [u.text || '']).map((x) => (typeof x === 'string' ? Buffer.from(x) : x));
  let i = 0;
  return new Response(new ReadableStream({ pull(c) {
    if (u.failAfter !== undefined && i === u.failAfter) { c.error(new Error(u.failMessage || 'reset')); return; }
    if (i < chunks.length) { c.enqueue(chunks[i++]); return; }
    c.close();
  } }), { status: u.status, headers });
}

async function runFetch(c) {
  const sent = [];
  const brokerCalls = [];
  const broker = {
    async chatgptSession(user, rejected) { brokerCalls.push(['session', user, rejected]); const a = c.sessions.shift(); if (a === 'throw') throw new Error('down'); return a; },
    async chatgptReject(user, generation) { brokerCalls.push(['reject', user, generation]); return { ok: true }; },
    async chatgptCatalogue(user) { brokerCalls.push(['catalogue', user]); if (c.catalogue === 'throw') throw new Error('down'); return c.catalogue === undefined ? { ok: true, models: catalogue } : c.catalogue; },
    async chatgptDisconnect() { return { ok: true }; },
  };
  const db = { exec() {}, prepare() { return { get() { return null; } }; } };
  const t = mod.createChatGptOAuth({ db, broker, signal: () => 'sig-1', now: () => NOW, config: { codexBaseUrl: 'https://backend.invalid/api/' },
    fetchImpl: async (url, init) => { sent.push({ url, headers: init.headers, body: init.body, redirect: init.redirect }); const u = c.upstream.shift() || { status: 200, chunks: streams.text }; return upstreamResponse(u); } });
  let r;
  try { r = await t.fetchFor('qa-user', { conversation: c.conversation ?? null })(c.url || 'http://x/v1/chat/completions', { body: c.body }); } catch (e) { return { thrown: String(e && e.message), sent, brokerCalls }; }
  // The body as the chat loop reads it; a failed read keeps what came before it (readError).
  let text = '';
  let readError;
  if (r.body) {
    const reader = r.body.getReader();
    const dec = new TextDecoder();
    try { for (;;) { const { done, value } = await reader.read(); if (done) break; text += dec.decode(value, { stream: true }); } text += dec.decode(); } catch (e) { readError = String(e && e.message); }
  }
  return { status: r.status, contentType: r.headers.get('content-type'), providerMessage: r.headers.get('x-noevia-provider-message'), text,
    ...(readError === undefined ? {} : { readError }), sent, brokerCalls };
}
const ok = (extra = {}) => ({ ok: true, session: { accessToken: 'tok-1', accountId: 'acct-1', fedRamp: false, expiresAt: NOW + 3_600_000, generation: 4, ...extra } });
const okSession = () => [ok()];
const body = (o) => JSON.stringify({ model: 'm-large', messages: [{ role: 'user', content: 'q' }], ...o });
const fetchCases = [
  { name: 'stream', body: body({ stream: true }), sessions: okSession(), upstream: [{ status: 200, chunks: streams.text }], conversation: 'chat-1' },
  { name: 'complete', body: body({}), sessions: okSession(), upstream: [{ status: 200, chunks: streams.tools }] },
  { name: 'completeReasoning', body: body({}), sessions: okSession(), upstream: [{ status: 200, chunks: streams.reasoning }] },
  { name: 'streamTools', body: body({ stream: true }), sessions: okSession(), upstream: [{ status: 200, chunks: streams.tools }] },
  { name: 'streamErrorFirst', body: body({ stream: true }), sessions: okSession(), upstream: [{ status: 200, chunks: streams.errorKnown }] },
  { name: 'streamFailedLater', body: body({ stream: true }), sessions: okSession(), upstream: [{ status: 200, chunks: streams.failed }] },
  { name: 'streamEarly', body: body({ stream: true }), sessions: okSession(), upstream: [{ status: 200, chunks: streams.earlyEnd }] },
  { name: 'streamEmpty', body: body({ stream: true }), sessions: okSession(), upstream: [{ status: 200, chunks: streams.empty }] },
  { name: 'completeFailed', body: body({}), sessions: okSession(), upstream: [{ status: 200, chunks: streams.failedUsage }] },
  { name: 'completeEarly', body: body({}), sessions: okSession(), upstream: [{ status: 200, chunks: streams.earlyEnd }] },
  { name: 'notChat', url: 'http://x/v1/images/generations', body: body({}), sessions: okSession(), upstream: [] },
  { name: 'badUrl', url: 'not a url', body: body({}), sessions: okSession(), upstream: [] },
  { name: 'chatQuery', url: 'http://x/v1/chat/completions?x=1', body: body({}), sessions: okSession(), upstream: [] },
  { name: 'badJson', body: '{', sessions: okSession(), upstream: [] },
  { name: 'noModel', body: JSON.stringify({ messages: [] }), sessions: okSession(), upstream: [] },
  { name: 'numberBody', body: '5', sessions: okSession(), upstream: [] },
  { name: 'notOnAccount', body: body({ model: 'm-other' }), sessions: okSession(), upstream: [] },
  { name: 'catalogueDown', body: body({ model: 'm-other' }), catalogue: 'throw', sessions: okSession(), upstream: [] },
  { name: 'catalogueRefused', body: body({ model: 'm-other', reasoning_effort: 'xhigh' }), catalogue: { ok: false, error: 'no' }, sessions: okSession(), upstream: [] },
  { name: 'catalogueEmpty', body: body({ model: 'm-other', reasoning_effort: 'high' }), catalogue: { ok: true, models: [] }, sessions: okSession(), upstream: [] },
  { name: 'catalogueMany', body: body({ model: 'zz' }), catalogue: { ok: true, models: Array.from({ length: 10 }, (_, i) => ({ slug: i === 3 ? 7 : i === 4 ? null : `s${i}` })).concat([{}]) }, sessions: okSession(), upstream: [] },
];
for (const model of ['m-large', 'm-small', 'm-none', 'm-auto']) {
  for (const reasoning_effort of [undefined, 'low', 'xhigh', 'max', 'default', 'minimal']) {
    fetchCases.push({ name: `effort-${model}-${reasoning_effort}`, body: body({ model, reasoning_effort }), sessions: okSession(), upstream: [{ status: 200, chunks: streams.text }], conversation: 'c' });
  }
}
fetchCases.push(
  { name: 'unknownEffortNoList', body: body({ model: 'm-other', reasoning_effort: 'ultra' }), catalogue: 'throw', sessions: okSession(), upstream: [] },
  { name: 'basicEffortNoList', body: body({ model: 'm-other', reasoning_effort: 'low', response_format: { type: 'json_object' } }), catalogue: 'throw', sessions: okSession(), upstream: [{ status: 200, chunks: streams.text }] },
  { name: 'fedRamp', body: body({}), sessions: [ok({ fedRamp: true })], upstream: [{ status: 200, chunks: streams.text }], conversation: 'room' },
  { name: 'retry401', body: body({}), sessions: [ok(), ok({ accessToken: 'tok-2', generation: 5 })], upstream: [{ status: 401, text: 'no' }, { status: 200, chunks: streams.text }] },
  { name: 'twice401', body: body({}), sessions: [ok(), ok({ accessToken: 'tok-2', generation: 5 })], upstream: [{ status: 401, text: 'no' }, { status: 401, text: 'no' }] },
  { name: 'brokerDown', body: body({}), sessions: ['throw'], upstream: [] },
  { name: 'brokerRefused', body: body({}), sessions: [{ ok: false, error: 'Sign in first', status: 409, code: 'disconnected' }], upstream: [] },
  { name: 'brokerOddStatus', body: body({}), sessions: [{ ok: false, status: 200.5 }], upstream: [] },
  { name: 'retryBrokerDown', body: body({}), sessions: [ok(), 'throw'], upstream: [{ status: 401, text: '' }] },
);
const upstreamErrors = [[500, ''], [500, '{"error":{"message":"internal"}}'], [429, '{"detail":"slow down"}'], [429, ''], [403, 'You have hit your usage limit'],
  [400, '{"error":{"type":"usage_not_included"}}'], [402, '{"error":{"code":"credit_balance_exhausted"}}'], [503, '{"message":"' + 'x'.repeat(400) + '"}'],
  [429, '{"error":{"type":"usage_limit_reached","plan_type":"team","resets_at":1800007200}}'], [404, '"just a string"'], [418, '{"error":"flat","detail":"d"}'], [502, '[1,2]']];
for (const [status, text] of upstreamErrors) fetchCases.push({ name: `upstream-${status}-${text.slice(0, 20)}`, body: body({}), sessions: okSession(), upstream: [{ status, text }] });
// The edges fetchFor handles inline (noevia#2699 item 3): labelled pass-through, body-less answers, the
// 64 KiB cap on a failed answer, failed reads at every stage, a send that rejects, odd catalogues.
const pad = (head, tail, size) => head + ' '.repeat(size - Buffer.byteLength(head) - Buffer.byteLength(tail)) + tail;
const capJson = (size) => pad('{"error":{"message":"m"}', '}', size);
const edgeCases = [
  { name: 'labelledError', upstream: [{ status: 409, text: '{"error":{"message":"held"}}', labelled: true }] },
  { name: 'labelledOk', upstream: [{ status: 200, chunks: streams.text, labelled: true }] },
  { name: 'labelledJson', upstream: [{ status: 409, text: '{"error":{"message":"held"}}', labelled: true, contentType: 'application/json; charset=utf-8' }] },
  { name: 'labelledAfter401', sessions: [ok(), ok({ accessToken: 'tok-2' })], upstream: [{ status: 401, text: '' }, { status: 503, text: 'busy', labelled: true }] },
  { name: 'noBody200', upstream: [{ status: 200, nobody: true }] },
  { name: 'noBody200Stream', stream: true, upstream: [{ status: 200, nobody: true }] },
  { name: 'noBody500', upstream: [{ status: 500, nobody: true }] },
  { name: 'noBody429', upstream: [{ status: 429, nobody: true }] },
  { name: 'noBody204', upstream: [{ status: 204, nobody: true }] },
  { name: 'noBody304', upstream: [{ status: 304, nobody: true }] },
  { name: 'capExact', upstream: [{ status: 500, text: capJson(65536) }] },
  { name: 'capOver', upstream: [{ status: 500, text: capJson(65537) }] },
  { name: 'capOverChunks', upstream: [{ status: 500, chunks: [capJson(65537).slice(0, 30000), capJson(65537).slice(30000, 60000), capJson(65537).slice(60000)] }] },
  { name: 'capExactThenMore', upstream: [{ status: 500, chunks: [capJson(65536), ' '] }] },
  { name: 'capExactThenEmpty', upstream: [{ status: 500, chunks: [capJson(65536), ''] }] },
  { name: 'capLimitInside', upstream: [{ status: 403, text: 'x'.repeat(65000) + 'usage limit' }] },
  { name: 'capLimitCut', upstream: [{ status: 403, text: 'x'.repeat(65530) + 'usage limit' }] },
  { name: 'capMultibyte', upstream: [{ status: 418, text: pad('{"detail":"d"', '}', 65535) + 'é' }] },
  { name: 'errorBodyFails', upstream: [{ status: 500, chunks: ['{"error":{"message":"m"}}'], failAfter: 1, failMessage: 'reset' }] },
  { name: 'errorBodyFailsAtOnce', upstream: [{ status: 429, chunks: [], failAfter: 0 }] },
  { name: 'streamFailFirst', stream: true, upstream: [{ status: 200, chunks: streams.text, failAfter: 0, failMessage: 'socket hang up ' + 'z'.repeat(300) }] },
  { name: 'streamFailMidEvent', stream: true, upstream: [{ status: 200, chunks: ['data: {"type":"response.output_text.delta",'], failAfter: 1, failMessage: 'reset' }] },
  { name: 'streamFailLater', stream: true, upstream: [{ status: 200, chunks: streams.text, failAfter: 1, failMessage: 'reset' }] },
  { name: 'streamFailAfterFinish', stream: true, upstream: [{ status: 200, chunks: streams.text, failAfter: streams.text.length, failMessage: 'reset' }] },
  { name: 'completeFailLater', upstream: [{ status: 200, chunks: streams.text, failAfter: 1, failMessage: 'reset' }] },
  { name: 'completeFailFirst', upstream: [{ status: 200, chunks: [], failAfter: 0, failMessage: 'gone' }] },
  { name: 'completeFailAfterError', upstream: [{ status: 200, chunks: streams.failed, failAfter: streams.failed.length, failMessage: 'late' }] },
  { name: 'sendThrows', upstream: [{ throws: 'connect refused' }] },
  { name: 'retryThenThrows', sessions: [ok(), ok({ accessToken: 'tok-2' })], upstream: [{ status: 401, text: '' }, { throws: 'second refused' }] },
  { name: 'bomStream', stream: true, upstream: [{ status: 200, chunks: streams.bom }] },
  { name: 'bomComplete', upstream: [{ status: 200, chunks: streams.bomSplit }] },
  { name: 'bomErrorBody', upstream: [{ status: 500, text: '\ufeff{"error":{"message":"m"}}' }] },
  { name: 'catalogueObject', catalogue: { ok: true, models: {} } },
  { name: 'catalogueString', catalogue: { ok: true, models: 'm-large' }, effort: 'xhigh' },
  { name: 'catalogueNumber', catalogue: { ok: true, models: 0 } },
  { name: 'catalogueMissing', catalogue: { ok: true }, effort: 'ultra' },
  { name: 'catalogueNull', catalogue: { ok: true, models: null } },
  // noevia#2717 item 4: a null catalogue entry (core throws at m.slug; the port skips it and answers as the
  // twin without it) and a request body of JSON null (core throws at body.model; the port's 400).
  { name: 'catalogueNullEntry', catalogue: { ok: true, models: [null, ...catalogue] } },
  { name: 'catalogueNullEntryAnswer' },
  { name: 'bodyNull', rawBody: 'null' },
  { name: 'catalogueNonArrayNoBody', catalogue: { ok: true, models: {} }, model: 'm-other', effort: 'low' },
];
for (const e of edgeCases) {
  fetchCases.push({ name: e.name, body: e.rawBody !== undefined ? e.rawBody : body({ ...(e.stream ? { stream: true } : {}), ...(e.effort ? { reasoning_effort: e.effort } : {}), ...(e.model ? { model: e.model } : {}) }),
    ...(e.catalogue ? { catalogue: e.catalogue } : {}), sessions: e.sessions || okSession(), upstream: e.upstream || [] });
}

// The lines core keeps as constants, cut from its source by exact patterns (fails if the shape moves).
function coreTexts() {
  const src = fs.readFileSync(SRC, 'utf8');
  const m = src.match(/\nconst MESSAGES = (\{[^}]*\});\n/);
  const n = src.match(/if \(type === 'usage_not_included'\) return '([^']*)';/);
  const s = src.match(/source: '([a-z]+)'/);
  if (!m || !n || !s) throw new Error('core source shape changed: MESSAGES / usage_not_included / source');
  const messages = new Function(`return ${m[1]}`)();
  return { label: 'Service', disconnected: messages.disconnected, reconnect: messages.reconnect, transient: messages.transient, notIncluded: n[1], source: s[1] };
}

(async () => {
  const streamOut = {};
  for (const [name, chunks] of Object.entries(streams)) streamOut[name] = { chunks: chunks.map((c) => Buffer.from(c).toString('base64')), parts: await partsOf(chunks) };
  const fetchOut = [];
  for (const c of fetchCases) {
    const input = JSON.parse(JSON.stringify({ ...c, sessions: c.sessions, upstream: c.upstream.map((u) => ({ status: u.status, chunks: (u.chunks || [u.text || '']).map((x) => Buffer.from(x).toString('base64')),
      ...(u.labelled ? { labelled: true } : {}), ...(u.contentType ? { contentType: u.contentType } : {}), ...(u.nobody ? { nobody: true } : {}), ...(u.failAfter === undefined ? {} : { failAfter: u.failAfter, failMessage: u.failMessage || 'reset' }),
      ...(u.throws ? { throws: u.throws } : {}) })) }));
    fetchOut.push({ input, output: await runFetch(c) });
  }
  const fixture = scrubDeep({
    source: { file: 'server/chatgpt-oauth.cjs', sha256: crypto.createHash('sha256').update(fs.readFileSync(SRC)).digest('hex') },
    now: NOW, userAgent: mod.USER_AGENT, texts: coreTexts(),
    requests, streams: streamOut, fetch: fetchOut,
  });
  const text = JSON.stringify(fixture, null, 1) + '\n';
  if (/chatgpt|codex|openai/i.test(text)) throw new Error('a product name survived the scrub');
  if (process.argv.includes('--check')) {
    if (fs.readFileSync(OUT, 'utf8') !== text) { console.error('fixture differs from a fresh recording'); process.exit(1); }
    console.log('fixture matches'); return;
  }
  fs.writeFileSync(OUT, text);
  console.log(`wrote ${OUT}: ${requests.length} requests, ${Object.keys(streamOut).length} streams, ${fetchOut.length} fetch cases`);
})();
