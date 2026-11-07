'use strict';
// Test-only Node oracle for the front's /api/chat-framing/suggest (chat port S4). For every case it answers the
// signed-in request with core's own routes/chat-framing.cjs and http.cjs (json, readJson) at the checkout given
// (CI: tests/fixtures/framing-suggest-oracle.core-ref, the last core whose route answers the suggestion), wired as
// server/index.cjs wired them: the `chat-frame:<user>` budget of 10 a minute on a fixed-window limiter (auth.cjs
// createRateLimiter's contract; it needs better-sqlite3 to load), a workspace that reads, and a decision faked as
// a pure function of what the route passed it, so the answer shows the message and chat id the route used. No
// model runs. A throw becomes http.cjs errorResponse, as index.cjs answers it.
//
//   node bins/noevia-server/tests/framing-suggest-oracle.cjs <core-checkout> <out.json> <front-core-checkout>
//
// The front core (contracts/http/front-core.ref, the core this front pairs with) gives the body limit of its
// loopback bridge (rust-chat-framing-suggest.cjs BRIDGE_BODY_LIMIT, or readJson's 1 MiB default when it
// names none); the fixture records it and the test's bridge refuses a longer body as that core does. A reply
// longer than 64 KiB is recorded as its SHA-256 and length.
//
// Deterministic; CI regenerates it and compares with tests/fixtures/node-framing-suggest.v1.json, which
// tests/framing_suggest.rs replays through the front against a bridge that applies the same fake decision.
const fs = require('node:fs');
const path = require('node:path');
const { Readable } = require('node:stream');

const crypto = require('node:crypto');
const [core, out, frontCore] = process.argv.slice(2);
if (!core || !out || !frontCore) throw new Error('Usage: node framing-suggest-oracle.cjs <core-checkout> <out.json> <front-core-checkout>');
const bridgeBodyLimit = require(path.join(path.resolve(frontCore), 'server', 'rust-chat-framing-suggest.cjs')).BRIDGE_BODY_LIMIT ?? 1024 * 1024;
const req = (m) => require(path.join(path.resolve(core), 'server', m));
const { createChatFramingRoutes } = req('routes/chat-framing.cjs');
const http = req('http.cjs');

/** The fake decision: what the front's test bridge answers for the same {message, chatId}. */
const decide = ({ message, chatId }) => ({ frame: { kind: 'question', echo: message, chatId }, reason: null });

// A body is { text } (UTF-8), { hex } (raw bytes), { big: n } ({"message":"<n a's>"}) or
// { chatId: n, byte } ({"message":"m","chatId":"<n of that byte, in hex>"}).
function bytesOf(spec) {
  if (spec.text !== undefined) return Buffer.from(spec.text, 'utf8');
  if (spec.hex !== undefined) return Buffer.from(spec.hex, 'hex');
  if (spec.chatId !== undefined) return Buffer.concat([Buffer.from('{"message":"m","chatId":"'), Buffer.alloc(spec.chatId, Number.parseInt(spec.byte, 16)), Buffer.from('"}')]);
  return Buffer.from(`{"message":"${'a'.repeat(spec.big)}"}`, 'utf8');
}

function limiter() {
  const map = new Map();
  return (key, limit) => { const n = (map.get(key) || 0) + 1; map.set(key, n); return n > limit; };
}

async function answer(routes, method, spec) {
  const r = Readable.from(spec ? [bytesOf(spec)] : []);
  Object.assign(r, { method, url: '/api/chat-framing/suggest', headers: {} });
  const res = { headers: {}, chunks: [] };
  res.writeHead = (status, h = {}) => { res.status = status; for (const [k, v] of Object.entries(h)) res.headers[k.toLowerCase()] = v; return res; };
  res.setHeader = (k, v) => { res.headers[k.toLowerCase()] = v; };
  res.end = (c) => { if (c !== undefined) res.chunks.push(Buffer.from(c)); };
  try {
    await routes(r, res, { path: '/api/chat-framing/suggest', authn: { user: { id: 'u-oracle', role: 'member' } } });
  } catch (err) {
    const failure = http.errorResponse(err);
    http.json(res, failure.status, failure.body);
  }
  const body = Buffer.concat(res.chunks);
  return {
    status: res.status,
    headers: { 'content-type': res.headers['content-type'], 'cache-control': res.headers['cache-control'] },
    ...(body.length > 64 * 1024
      ? { bodySha256: crypto.createHash('sha256').update(body).digest('hex'), bodyLength: body.length }
      : { body: body.toString('base64') }),
  };
}

function wired() {
  const rate = limiter();
  return createChatFramingRoutes({
    json: http.json, readJson: http.readJson, settings: { get: () => ({}), save: () => ({}) },
    framing: { suggest: async (args) => decide(args) },
    rateLimited: (userId) => rate(`chat-frame:${userId}`, 10),
    workspace: () => ({ projects: [{ id: 'p', name: 'Synthetic' }], chats: [{ id: 'c', title: 'Synthetic' }] }),
  });
}

const t = (text) => ({ text });
const CASES = [
  ...['GET', 'PUT', 'DELETE', 'OPTIONS', 'PATCH'].map((m) => ({ id: `method-${m}`, steps: [[m, null]] })),
  { id: 'empty-body', steps: [['POST', t('')]] },
  { id: 'empty-object', steps: [['POST', t('{}')]] },
  { id: 'null', steps: [['POST', t('null')]] },
  { id: 'array', steps: [['POST', t('[]')]] },
  { id: 'string', steps: [['POST', t('"hi"')]] },
  { id: 'number', steps: [['POST', t('5')]] },
  { id: 'message-number', steps: [['POST', t('{"message":5}')]] },
  { id: 'message-null', steps: [['POST', t('{"message":null}')]] },
  { id: 'invalid-json', steps: [['POST', t('{')]] },
  { id: 'trailing-garbage', steps: [['POST', t('{"message":"a"} x')]] },
  { id: 'bom', steps: [['POST', t('﻿{"message":"a"}')]] },
  { id: 'whitespace', steps: [['POST', t(' \n{"message":"a"}\t ')]] },
  { id: 'plain', steps: [['POST', t('{"message":"Plan the garden"}')]] },
  { id: 'empty-message', steps: [['POST', t('{"message":""}')]] },
  { id: 'chat-id', steps: [['POST', t('{"message":"m","chatId":"c9"}')]] },
  { id: 'chat-id-empty', steps: [['POST', t('{"chatId":"","message":"m"}')]] },
  { id: 'chat-id-number', steps: [['POST', t('{"message":"m","chatId":7}')]] },
  { id: 'chat-id-null', steps: [['POST', t('{"message":"m","chatId":null}')]] },
  { id: 'duplicate-keys', steps: [['POST', t('{"message":"a","message":"b"}')]] },
  { id: 'extra-fields', steps: [['POST', t('{"projects":[{"id":"injected"}],"message":"m","__proto__":{"x":1}}')]] },
  { id: 'unicode', steps: [['POST', t('{"message":"İ ß Σ — 日本 😀 \\u0000 \\"q\\""}')]] },
  { id: 'lone-surrogate', steps: [['POST', t('{"message":"x\\udc00y\\ud83d"}')]] },
  { id: 'invalid-utf8', steps: [['POST', { hex: Buffer.concat([Buffer.from('{"message":"a'), Buffer.from([0xff, 0xc3, 0x28, 0xe2, 0x82]), Buffer.from('b"}')]).toString('hex') }]] },
  // What JSON.parse accepts but a parsed tree with limits could not hold: any nesting, any number, every character.
  { id: 'deep-array-200', steps: [['POST', t(`{"x":${'['.repeat(200)}1${']'.repeat(200)},"message":"m"}`)]] },
  { id: 'deep-object-5000', steps: [['POST', t(`{"message":"m","x":${'{"a":'.repeat(5000)}null${'}'.repeat(5000)}}`)]] },
  { id: 'deep-top-level', steps: [['POST', t(`${'['.repeat(3000)}${']'.repeat(3000)}`)]] },
  { id: 'number-overflow-field', steps: [['POST', t('{"n":1e400,"m":-1e400,"message":"m","z":1e-400}')]] },
  { id: 'number-overflow-message', steps: [['POST', t('{"message":1e400}')]] },
  { id: 'number-overflow-chat-id', steps: [['POST', t('{"message":"m","chatId":1e400}')]] },
  { id: 'number-overflow-top', steps: [['POST', t('1e400')]] },
  { id: 'number-long-digits', steps: [['POST', t(`{"message":"m","n":${'9'.repeat(400)}.5e-10}`)]] },
  { id: 'reserved-chars-message', steps: [['POST', t('{"message":"\u{10F800}a\u{10FFFF}"}')]] },
  { id: 'reserved-chars-escaped', steps: [['POST', t('{"message":"\\udbfe\\udc00\\udbff\\udfff"}')]] },
  { id: 'reserved-chars-chat-id', steps: [['POST', t('{"message":"m","chatId":"\u{10FFFF}id"}')]] },
  { id: 'reserved-chars-key', steps: [['POST', t('{"\u{10F800}":"x","message":"m"}')]] },
  { id: 'reserved-char-straddles-cap', steps: [['POST', t(`{"message":"${'m'.repeat(3999)}\u{10FFFF}tail"}`)]] },
  { id: 'reserved-char-at-cap', steps: [['POST', t(`{"message":"${'m'.repeat(3998)}\u{10F800}tail"}`)]] },
  { id: 'long', steps: [['POST', { big: 5000 }]] },
  { id: 'exactly-cap', steps: [['POST', { big: 4000 }]] },
  { id: 'emoji-straddles-cap', steps: [['POST', t(`{"message":"${'m'.repeat(3999)}😀tail"}`)]] },
  { id: 'emoji-before-cap', steps: [['POST', t(`{"message":"${'m'.repeat(3998)}😀tail"}`)]] },
  { id: 'one-mebibyte', steps: [['POST', { big: 1024 * 1024 - 14 }]] },
  { id: 'over-one-mebibyte', steps: [['POST', { big: 1024 * 1024 - 13 }]] },
  // A body at the limit the front re-sends with the user added: the chat id is kept whole (review P2), and an
  // invalid byte in it triples as U+FFFD. One byte over is the public limit's answer.
  { id: 'one-mebibyte-chat-id', steps: [['POST', { chatId: 1024 * 1024 - 27, byte: '63' }]] },
  { id: 'over-one-mebibyte-chat-id', steps: [['POST', { chatId: 1024 * 1024 - 26, byte: '63' }]] },
  { id: 'one-mebibyte-invalid-utf8-chat-id', steps: [['POST', { chatId: 1024 * 1024 - 27, byte: 'ff' }]] },
  { id: 'over-one-mebibyte-invalid-utf8-chat-id', steps: [['POST', { chatId: 1024 * 1024 - 26, byte: 'ff' }]] },
  // The budget: counted before the body and only for a POST; the 11th in the minute is refused.
  { id: 'budget', steps: [
    ['GET', null], ...Array.from({ length: 4 }, () => ['POST', t('{"message":"m"}')]), ['POST', t('{')], ['POST', t('{}')],
    ...Array.from({ length: 3 }, () => ['POST', t('{"message":"m"}')]), ['PUT', null], ['POST', t('{"message":"m"}')], ['POST', t('{"message":"m"}')], ['POST', t('{')],
  ] },
];

(async () => {
  const cases = [];
  for (const c of CASES) {
    const routes = wired();
    const steps = [];
    for (const [method, spec] of c.steps) steps.push({ method, body: spec, node: await answer(routes, method, spec) });
    cases.push({ id: c.id, steps });
  }
  fs.writeFileSync(out, `${JSON.stringify({ v: 1, bridgeBodyLimit, cases }, null, 1)}\n`);
})().catch((e) => { console.error(e); process.exit(1); });
