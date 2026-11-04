#!/usr/bin/env node
'use strict';
// Oracle for crates/server-diary's local exchange, from raw sidecar text through Core's own code:
//  - failures: the non-streamed failure answer, run from the handler's own source at the commit
//    before the move (routes/diary.cjs, `const error = r.body?.detail …`), on `fetchJson`'s body;
//  - refusals, legacy, frames: Core's proxyDiaryStream (server/diary-stream.cjs) over a stubbed
//    `fetch` answering the given status, headers and text; every frame after the first status;
//  - numbers, wide: JSON.stringify(JSON.parse(text)).
// Synthetic values only.
//   node tools/gen-diary-local-exchange-raw-fixtures.cjs <noevia-core checkout> <commit that moved the route>
//     > crates/server-diary/tests/fixtures/local-exchange-node-raw.json
const path = require('node:path');
const { execFileSync } = require('node:child_process');

const [core, moved] = process.argv.slice(2);
if (!core || !moved) throw new Error('usage: gen-diary-local-exchange-raw-fixtures.cjs <core> <commit>');
const http = require(path.resolve(core, 'server/http.cjs'));
const { proxyDiaryStream } = require(path.resolve(core, 'server/diary-stream.cjs'));

// The removed handler's failure answer, verbatim.
const handler = execFileSync('git', ['-C', core, 'show', `${moved}^:server/routes/diary.cjs`], { encoding: 'utf8' });
const from = handler.indexOf('const error = r.body?.detail');
const lastLine = "return json(res, r.status, { error, ...(r.body?.code === 'storageLoginRejected'";
const to = handler.indexOf('\n', handler.indexOf(lastLine, from));
if (from < 0 || to < 0) throw new Error('failure block not found');
const failureBlock = new Function('r', 'res', 'json', handler.slice(from, to));

function stubFetch(status, text, headers) {
  globalThis.fetch = async () => new Response(text, { status, headers });
}

function response() {
  const writes = [];
  const res = {
    status: 0, headers: {}, writes, destroyed: false, writableEnded: false,
    on() {}, off() {},
    writeHead(code, headers) { this.status = code; this.headers = headers || {}; },
    write(chunk) { writes.push(String(chunk)); return true; },
    end(chunk) { if (chunk !== undefined) writes.push(String(chunk)); this.writableEnded = true; },
  };
  return res;
}

async function failure(status, text) {
  stubFetch(status, text, {});
  const r = await http.fetchJson('http://diary.invalid/api/local-exchange', { method: 'POST', body: '{}' }, 5000);
  const res = response();
  failureBlock(r, res, http.json);
  return { status, text, expected: { status: res.status, body: res.writes.join(''), retryAfter: res.headers['Retry-After'] ?? null } };
}

async function stream(status, text, headers) {
  stubFetch(status, text, headers);
  const res = response();
  await proxyDiaryStream(res, 'http://diary.invalid/api/local-exchange', { method: 'POST', headers: {}, body: '{}' }, { heartbeatMs: 2 ** 30 });
  const frames = res.writes.filter(w => !w.startsWith(':'));
  if (!frames[0]?.includes('Connecting to diary companion')) throw new Error('no first status');
  return frames.slice(1);
}

const STRINGS = ['\u00851', '1\u0085', '᠎1', ' 1', ' 1', '﻿1', ' 1', '　1', '\u000b1',
  '\u000c1', ' \t1\r\n', '​1', ' 1', ' 1', ' 1', '0x10', '0X1a', '-0x10', '+0x10', '0b11', '0o17',
  '0x', '0b2', '1e3', '.5', '5.', '+.5', '-5', '1_0', '١', '1e', 'e5', '--1', '+-1', '', ' ', '.', '+', '0.0000001',
  '86400.5', '1e5', '2.5', '1 2', 'Infinity', '-Infinity', '+Infinity', 'infinity', 'inf', '+inf', 'NaN', 'nan', '1e400',
  '00012', '1.', '1.e2', '.e2', '0x1p3', '1d', '1f', '٣'];

async function main() {
  const failures = [];
  const bodies = [
    [503, '{"detail":1e400,"error":"fallback"}'],
    [500, '{"detail":-1e400}'],
    [500, '{"detail":0,"error":1e400}'],
    [500, '{"detail":1e-400,"error":"underflow"}'],
    [500, '{"detail":-0.0,"error":"negative zero"}'],
    [500, '{"detail":"","error":false,"x":1}'],
    [500, '{"detail":"a","detail":1e400}'],
    [500, '{"detail":2.98023223876953125e-8}'],
    [424, '{"detail":{"a":[1e400,2.98023223876953125e-8]},"code":"storageLoginRejected"}'],
    [503, '{"code":"storageThrottled","retryAfter":1e400}'],
    [503, '{"code":"storageThrottled","retryAfter":-1e400}'],
    [503, '{"code":"storageThrottled","retryAfter":[1e400]}'],
    [503, '{"code":"storageThrottled","retryAfter":[5]}'],
    [503, '{"code":"storageThrottled","retryAfter":["7"]}'],
    [503, '{"code":"storageThrottled","retryAfter":{"a":1}}'],
    [503, '{"code":"storageThrottled","retryAfter":true}'],
    [503, '{"code":"storageThrottled","retryAfter":null}'],
    [503, '{"code":"storageThrottled"}'],
    [502, 'Bad gateway <html>'],
    [502, ''],
    [500, '1e400'],
    ...STRINGS.map(s => [503, JSON.stringify({ code: 'storageThrottled', retryAfter: s })]),
  ];
  for (const [status, text] of bodies) failures.push(await failure(status, text));

  const refusals = [];
  const refusal = async (status, text, header) => refusals.push({
    status, text, retryHeader: header,
    expected: await stream(status, text, header === null ? {} : { 'retry-after': header }),
  });
  await refusal(429, '{"code":"storageThrottled","retryAfter":1e400}', '60');
  await refusal(429, '{"code":"storageThrottled","retryAfter":-1e400}', '60');
  await refusal(429, '{"code":"storageThrottled","retryAfter":[1e400]}', '60');
  await refusal(429, '{"code":"storageThrottled","retryAfter":null}', '60');
  await refusal(429, '{"code":"storageThrottled"}', '60');
  await refusal(429, '{"code":"storageThrottled","retryAfter":1e400}', null);
  await refusal(503, '1e400', '5');
  await refusal(503, '{"code":"storageUpstream","retryAfter":3}', null);
  for (const s of STRINGS) await refusal(429, JSON.stringify({ code: 'storageThrottled', retryAfter: s }), '60');
  // Header values as a fetch Response holds them (Latin-1; leading and trailing HTTP white space
  // removed by Headers).
  for (const h of [' 7', '\u00857', '7', '0x10', '7.5', '1e400', '-1', 'abc', ' ', '+7', '7 ']) {
    await refusal(429, '{"code":"storageThrottled"}', h);
  }

  const legacy = [];
  for (const text of [
    '{"choices":[{"message":{"content":1e400}}]}',
    '{"choices":[{"message":{"content":-1e400}}]}',
    '{"choices":[{"message":{"content":1e-400}}]}',
    '{"choices":[{"message":{"content":"hi","reasoning_content":1e400}}],"diary":{"saved":1e400,"n":2.98023223876953125e-8}}',
    '{"choices":[{"message":{"content":"hi","reasoning_content":0}}],"diary":[1e400]}',
    '{"reply":"ok","reasoning":1e400,"extra":1e400}',
    '{"reply":1e400}',
    '{"choices":{"0":{"message":{"content":"from an object"}}},"diary":{"a":1}}',
    '{"choices":"x","reply":"from the reply"}',
    '{"choices":[],"reply":"empty choices"}',
  ]) legacy.push({ text, expected: await stream(200, text, { 'content-type': 'application/json' }) });

  const frames = [];
  for (const list of [
    ['data: {"type":"error","reason":"x y","n":1e400}\n\n'],
    ['data: {"type":"error","reason":"bad reason!","x":2.98023223876953125e-8,"y":-1e400}\n\n'],
    ['data: {"type":"status","text":"Opening diary context…","n":1e400}\n\n', 'data: {"type":"error","code":"other","reason":"chat_timeout","n":1e400}\n\n'],
    ['data: {"type":"status","text":"Generating companion response…"}\n\n', 'data: {"type":"error","reason":"chat_timeout","n":1e400}\n\n'],
    ['data: {"type":"answer","text":1e400}\n\n', 'data: {"type":"done"}\n\n'],
  ]) {
    frames.push({ frames: list, expected: await stream(200, list.join(''), { 'content-type': 'text/event-stream' }) });
  }

  const numbers = [];
  const view = new DataView(new ArrayBuffer(8));
  const literals = ['2.98023223876953125e-8', '-2.98023223876953125e-8', '2.9802322387695312e-8',
    '2.9802322387695313e-8', '5e-324', '1e21', '1e-7', '0.1', '1E400', '-1e400', '1e-400', '-0'];
  for (let e = -1074; e <= 1023; e += 1) {
    const x = 2 ** e;
    const s = x.toExponential();
    if (s.replace(/e.*/, '').replace('.', '').length === 17 && e % 3 === 0) literals.push(x.toExponential(19));
  }
  for (const text of literals) numbers.push({ text, expected: JSON.stringify(JSON.parse(text)) });
  numbers.push({ text: `[${literals.join(',')}]`, expected: JSON.stringify(JSON.parse(`[${literals.join(',')}]`)) });
  void view;

  // A wide object: shuffled index keys (some repeated), named keys between them.
  let seed = 12345;
  const rand = () => { seed = (seed * 1103515245 + 12345) % 2147483648; return seed; };
  const keys = [];
  for (let i = 0; i < 3000; i++) keys.push(String(rand() % 4294967295));
  for (let i = 0; i < 200; i++) keys.splice(rand() % keys.length, 0, `name${i}`);
  for (let i = 0; i < 100; i++) keys.splice(rand() % keys.length, 0, keys[rand() % keys.length]);
  keys.push('4294967295', '4294967294', '0', '01', '-1');
  const wideText = '{' + keys.map((k, i) => `${JSON.stringify(k)}:${i}`).join(',') + '}';
  const wide = [{ text: wideText, expected: JSON.stringify(JSON.parse(wideText)) }];

  const out = { generator: 'tools/gen-diary-local-exchange-raw-fixtures.cjs', core: moved, node: process.version, failures, refusals, legacy, frames, numbers, wide };
  const json = JSON.stringify(out);
  if (/\\ud[89ab][0-9a-f]{2}(?!\\ud[c-f])/i.test(json)) throw new Error('lone surrogate in fixture');
  process.stdout.write(json + '\n');
  process.stderr.write(`failures ${failures.length}, refusals ${refusals.length}, legacy ${legacy.length}, frames ${frames.length}, numbers ${numbers.length}; node ${process.version}\n`);
}
main().catch(e => { console.error(e); process.exit(1); });
