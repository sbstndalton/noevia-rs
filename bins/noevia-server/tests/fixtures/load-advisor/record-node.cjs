#!/usr/bin/env node
'use strict';
// R6 (sbstndalton/noevia#2550 item 2) load-advisor recorder: runs noevia-core's own
// server/load-advisor.cjs `createLoadAdvisor({ env })` (its real decision-endpoint.cjs client and
// the real load verdict through dav-parse.wasm) against a scripted stub decision service on
// 127.0.0.1, and writes what the stub received and what judge returned, per scenario.
//   DAV_PARSE_WASM=<dav-parse.wasm pinned by core's server/dav-parse.lock> \
//     node record-node.cjs <core checkout> <out file>
// Also writes SOURCES.sha256 beside this file (the core files it reads). The stub is a raw TCP
// server writing the scripted bytes, so the Rust replay (tests/load_advisor_replay.rs) serves the
// very same bytes. Synthetic QA data only; the only key is `qa-decision-key-1`.
const fs = require('node:fs'), path = require('node:path'), net = require('node:net'), crypto = require('node:crypto');

const [coreArg, outArg] = process.argv.slice(2);
if (!coreArg || !outArg) { console.error('usage: record-node.cjs <core checkout> <out file>'); process.exit(2); }
const core = path.resolve(coreArg);
const SOURCES = ['server/load-advisor.cjs', 'server/decision-endpoint.cjs', 'server/decision-models.cjs',
  'server/decision-typesafe.cjs', 'server/decision-wording.cjs', 'server/decision-qualification.cjs',
  'server/service-endpoint.cjs', 'server/public-fetch.cjs', 'server/ssrf.cjs', 'server/dav-parse-wasm.cjs',
  'server/dav-parse.lock'];
const sha = file => crypto.createHash('sha256').update(fs.readFileSync(path.join(core, file))).digest('hex');
const { createLoadAdvisor } = require(path.join(core, 'server', 'load-advisor.cjs'));
// The real verdict must load (a missing or unpinned module would make every judge null).
require(path.join(core, 'server', 'dav-parse-wasm.cjs')).loadVerdict({ cause: 'load', evidence: null });

// ── scenarios ────────────────────────────────────────────────────────────────────────────────
const PORT = '<port>';
const URL_ = `http://127.0.0.1:${PORT}`;
const LAYA = { COWORK_DECISION_URL: URL_ };
const KEY = 'qa-decision-key-1';
const KEV = { COWORK_DECISION_URL: URL_, COWORK_DECISION_MODEL: 'kev-0.8b', COWORK_DECISION_API_KEY: KEY };
const TEXT = 'qa-engine stopped: unexpected state 42 in worker qa-1';
const ASK = { cause: 'load', evidence: { status: 500, exitCode: 1, text: TEXT, crash: false } };
const ask = (text, extra = {}) => ({ cause: 'load', evidence: { status: 500, exitCode: 1, text, crash: false, ...extra } });
const json = (status, value) => ({ status, body: typeof value === 'string' ? value : JSON.stringify(value) });
const scores = (oom, load_failed, recall_failed, template, unknown) => ({ oom, load_failed, recall_failed, template, unknown });
const laya = (selected, s) => json(200, { selected, scores: s });
const USED = laya('oom', scores(0.7, 0.1, 0.1, 0.05, 0.05));
const typesafe = (p) => json(200, { answers: { d: { probabilities: p } } });
const BUSY = json(503, { error: 'busy' });
const answer = JSON.stringify({ selected: 'oom', scores: scores(0.7, 0.1, 0.1, 0.05, 0.05) });

const S = [];
const add = (name, env, replies, inputs, extra = {}) => S.push({ name, env, stub: { replies, ...(extra.stub || {}) }, calls: extra.calls || 'single', inputs });

add('rule-decides-not-asked', LAYA, [USED], [ask('CUDA error: out of memory while allocating a buffer')]);
add('answered-used', LAYA, [USED], [ASK]);
add('answered-ignored-low-confidence', LAYA, [laya('load_failed', scores(0.2, 0.5, 0.1, 0.1, 0.1))], [ASK]);
add('answered-unknown-label-ignored', LAYA, [laya('unknown', scores(0.025, 0.025, 0.025, 0.025, 0.9))], [ASK]);
add('answered-template-used', LAYA, [laya('template', scores(0.05, 0.05, 0.05, 0.8, 0.05))], [ASK]);
add('answered-confidence-at-threshold', LAYA, [laya('recall_failed', scores(0.1, 0.1, 0.6, 0.1, 0.1))], [ASK]);
add('answered-reply-fields-ignored', LAYA, [json(200, { selected: 'oom', scores: scores(0.7, 0.1, 0.1, 0.05, 0.05), confidence: 0.01, model: 'qa-model', revision: 'qa' })], [ASK]);
add('http-503', LAYA, [json(503, { error: 'busy' })], [ASK]);
add('http-422', LAYA, [json(422, { error: 'refused' })], [ASK]);
add('http-500', LAYA, [json(500, '')], [ASK]);
add('invalid-sum-off', LAYA, [laya('oom', scores(0.5, 0.1, 0.1, 0.1, 0.1))], [ASK]);
add('invalid-sum-just-over-tolerance', LAYA, [laya('oom', scores(0.7025, 0.1, 0.1, 0.05, 0.05))], [ASK]);
add('valid-sum-within-tolerance', LAYA, [laya('oom', scores(0.7015, 0.1, 0.1, 0.05, 0.05))], [ASK]);
add('invalid-selected-not-max', LAYA, [laya('oom', scores(0.3, 0.4, 0.1, 0.1, 0.1))], [ASK]);
add('invalid-selected-tie', LAYA, [laya('oom', scores(0.4, 0.4, 0.1, 0.05, 0.05))], [ASK]);
add('invalid-unknown-id', LAYA, [laya('gpu', scores(0.7, 0.1, 0.1, 0.05, 0.05))], [ASK]);
add('invalid-timeout-id', LAYA, [json(200, { selected: 'timeout', scores: { ...scores(0.1, 0.1, 0.05, 0.05, 0), timeout: 0.7 } })], [ASK]);
add('invalid-missing-score', LAYA, [json(200, { selected: 'oom', scores: { oom: 0.7, load_failed: 0.2, recall_failed: 0.05, template: 0.05 } })], [ASK]);
add('invalid-extra-score', LAYA, [json(200, { selected: 'oom', scores: { ...scores(0.7, 0.1, 0.1, 0.05, 0.05), extra: 0 } })], [ASK]);
add('invalid-score-over-one', LAYA, [laya('oom', scores(1.2, -0.05, -0.05, -0.05, -0.05))], [ASK]);
add('invalid-score-string', LAYA, [json(200, { selected: 'oom', scores: { ...scores(0.7, 0.1, 0.1, 0.05, 0.05), oom: '0.7' } })], [ASK]);
add('invalid-scores-array', LAYA, [json(200, { selected: 'oom', scores: [0.7, 0.1, 0.1, 0.05, 0.05] })], [ASK]);
add('invalid-scores-missing', LAYA, [json(200, { selected: 'oom' })], [ASK]);
add('invalid-selected-number', LAYA, [json(200, { selected: 1, scores: scores(0.7, 0.1, 0.1, 0.05, 0.05) })], [ASK]);
add('invalid-score-infinite', LAYA, [json(200, '{"selected":"oom","scores":{"oom":1e400,"load_failed":0.1,"recall_failed":0.1,"template":0.05,"unknown":0.05}}')], [ASK]);
add('duplicate-keys-last-wins', LAYA, [json(200, '{"selected":"load_failed","scores":{"oom":0.1,"load_failed":0.1,"recall_failed":0.05,"template":0.05,"unknown":0.05,"oom":0.05,"load_failed":0.75},"selected":"oom"}')], [ASK]);
add('duplicate-keys-valid', LAYA, [json(200, '{"selected":"oom","scores":{"oom":0.1,"load_failed":0.1,"recall_failed":0.05,"template":0.05,"unknown":0.05,"oom":0.7}}')], [ASK]);
add('deeply-nested-extra-field', LAYA, [json(200, `{"selected":"oom","scores":${JSON.stringify(scores(0.7, 0.1, 0.1, 0.05, 0.05))},"x":${'['.repeat(300)}${']'.repeat(300)}}`)], [ASK]);
add('not-json', LAYA, [json(200, 'not json')], [ASK]);
add('json-null', LAYA, [json(200, 'null')], [ASK]);
add('json-array', LAYA, [json(200, '[]')], [ASK]);
add('json-string', LAYA, [json(200, '"oom"')], [ASK]);
add('bom-before-json', LAYA, [json(200, '﻿' + answer)], [ASK]);
add('empty-body', LAYA, [json(200, '')], [ASK]);
add('whitespace-body', LAYA, [json(200, ' \n\t ')], [ASK]);
add('no-content-204', LAYA, [{ status: 204, body: '' }], [ASK]);
add('body-exactly-8192-bytes', LAYA, [json(200, answer + ' '.repeat(8192 - Buffer.byteLength(answer)))], [ASK]);
add('body-8193-bytes-too-large', LAYA, [json(200, answer + ' '.repeat(8193 - Buffer.byteLength(answer)))], [ASK]);
add('body-much-too-large', LAYA, [{ status: 200, bodyRepeat: { text: 'qa ', count: 20000 } }], [ASK]);
add('invalid-utf8-body', LAYA, [{ status: 200, bodyHex: Buffer.concat([Buffer.from('{"selected":"oom","scores":{"oom":0.7,"load_failed":0.1,"recall_failed":0.1,"template":0.05,"unknown":0.05},"x":"'), Buffer.from([0xff, 0xc3]), Buffer.from('"}')]).toString('hex') }], [ASK]);
add('slow-reply-past-budget-timeout', LAYA, [{ ...USED, delayMs: 2600 }], [ASK]);
add('slow-reply-within-budget', LAYA, [{ ...USED, delayMs: 300 }], [ASK]);
add('unconfigured-url-unavailable', {}, [USED], [ASK]);
add('empty-url-unavailable', { COWORK_DECISION_URL: '' }, [USED], [ASK]);
add('public-ip-literal-unavailable', { COWORK_DECISION_URL: 'http://8.8.8.8:8040' }, [USED], [ASK]);
add('metadata-ip-unavailable', { COWORK_DECISION_URL: 'http://169.254.169.254' }, [USED], [ASK]);
add('url-with-path-unavailable', { COWORK_DECISION_URL: `${URL_}/v1` }, [USED], [ASK]);
add('url-with-query-unavailable', { COWORK_DECISION_URL: `${URL_}/?x=1` }, [USED], [ASK]);
add('url-with-login-unavailable', { COWORK_DECISION_URL: `http://qa:qa@127.0.0.1:${PORT}` }, [USED], [ASK]);
add('url-not-http-unavailable', { COWORK_DECISION_URL: `ftp://127.0.0.1:${PORT}` }, [USED], [ASK]);
add('url-trailing-slash', { COWORK_DECISION_URL: `${URL_}/` }, [USED], [ASK]);
add('url-surrounding-space', { COWORK_DECISION_URL: ` ${URL_}\n` }, [USED], [ASK]);
add('kev-typesafe-key-tempered', KEV, [typesafe(scores(0.55, 0.1125, 0.1125, 0.1125, 0.1125))], [ASK]);
add('kev-typesafe-renormalised', KEV, [typesafe(scores(1.2, 0.2, 0.2, 0.2, 0.2))], [ASK]);
add('kev-typesafe-low-after-temper', KEV, [typesafe(scores(0.3, 0.25, 0.15, 0.15, 0.15))], [ASK]);
add('kev-typesafe-exact-tie', KEV, [typesafe(scores(0.4, 0.4, 0.1, 0.05, 0.05))], [ASK]);
add('kev-typesafe-missing-option', KEV, [typesafe({ oom: 0.7, load_failed: 0.1, recall_failed: 0.1, template: 0.1 })], [ASK]);
add('kev-typesafe-negative', KEV, [typesafe(scores(0.9, -0.1, 0.1, 0.05, 0.05))], [ASK]);
add('kev-typesafe-all-zero', KEV, [typesafe(scores(0, 0, 0, 0, 0))], [ASK]);
add('kev-typesafe-laya-shaped-answer', KEV, [USED], [ASK]);
add('kev-typesafe-zero-probabilities-floor', KEV, [typesafe(scores(0.97, 0, 0.01, 0.01, 0.01))], [ASK]);
add('kev-key-with-space-not-sent', { ...KEV, COWORK_DECISION_API_KEY: 'qa key 1' }, [typesafe(scores(0.55, 0.1125, 0.1125, 0.1125, 0.1125))], [ASK]);
add('kev-key-trimmed', { ...KEV, COWORK_DECISION_API_KEY: '  qa-decision-key-1 \t' }, [typesafe(scores(0.55, 0.1125, 0.1125, 0.1125, 0.1125))], [ASK]);
add('kev-no-key', { COWORK_DECISION_URL: URL_, COWORK_DECISION_MODEL: 'kev-0.8b' }, [typesafe(scores(0.55, 0.1125, 0.1125, 0.1125, 0.1125))], [ASK]);
add('kev-model-id-case-and-space', { ...KEV, COWORK_DECISION_MODEL: ' KEV-0.8B ' }, [typesafe(scores(0.55, 0.1125, 0.1125, 0.1125, 0.1125))], [ASK]);
add('laya-model-key-not-sent', { ...LAYA, COWORK_DECISION_MODEL: 'laya', COWORK_DECISION_API_KEY: KEY }, [USED], [ASK]);
add('unknown-model-is-laya', { ...LAYA, COWORK_DECISION_MODEL: 'qa-model', COWORK_DECISION_API_KEY: KEY }, [USED], [ASK]);
add('busy-second-question', LAYA, [{ ...USED, delayMs: 300 }, USED], [ASK, ask('qa-engine stopped: second question')], { calls: 'concurrent' });
add('reset-then-success-one-retry', LAYA, [{ reset: true }, USED], [ASK]);
add('reset-twice-error', LAYA, [{ reset: true }, { reset: true }, USED], [ASK]);
add('connection-refused-error', LAYA, [], [ASK], { stub: { closed: true } });
add('redirect-302-error', LAYA, [{ status: 302, headers: [['Location', `http://127.0.0.1:${PORT}/v1/decisions`]], body: '' }, USED], [ASK]);
add('redirect-307-error', LAYA, [{ status: 307, headers: [['Location', '/v1/decisions']], body: '' }, USED], [ASK]);
add('lane-held-after-timeout', LAYA, [{ ...USED, delayMs: 2600 }, USED], [ASK, ask('qa-engine stopped: asked after a timeout')], { calls: 'sequential', stub: { singleWorker: true } });
add('lane-hold-cap-hang', LAYA, [{ hang: true }, USED], [ASK, ask('qa-engine stopped: asked behind a hang')], { calls: 'sequential', stub: { singleWorker: true } });
add('sequential-questions', LAYA, [USED, laya('template', scores(0.05, 0.05, 0.05, 0.8, 0.05))], [ASK, ask('qa-engine stopped: another question')], { calls: 'sequential', stub: { singleWorker: true } });
// The engine text as the question's state.
add('evidence-control-characters', LAYA, [USED], [ask('qa\u0000engine\u0007 stopped\r\nstate\t42\u000b\u001f\u007f\u0080\u0085\u009f end')]);
add('evidence-quotes-and-separators', LAYA, [USED], [ask('qa "engine" \\ stopped     </script> <&> 𝔘 é')]);
add('evidence-over-2048-units', LAYA, [USED], [ask('qa-engine stopped. '.repeat(160))]);
add('evidence-cut-surrogate-pair', LAYA, [USED], [ask('x'.repeat(2047) + '😀' + ' tail')]);
add('evidence-lone-surrogates', LAYA, [USED], [ask('qa-engine \ud800 stopped \udc00 end \udbff')]);
add('evidence-multibyte-long', LAYA, [USED], [ask('é'.repeat(3000))]);
add('evidence-whitespace-only', LAYA, [USED], [ask(' \n\t ')]);
add('evidence-text-not-string', LAYA, [USED], [{ cause: 'load', evidence: { status: 500, exitCode: 1, text: 42 } }]);
add('evidence-missing', LAYA, [USED], [{ cause: 'load' }]);
add('evidence-null', LAYA, [USED], [{ cause: 'load', evidence: null }]);
add('evidence-string', LAYA, [USED], [{ cause: 'load', evidence: 'qa-engine stopped' }]);
add('evidence-array', LAYA, [USED], [{ cause: 'load', evidence: ['qa-engine stopped'] }]);
add('evidence-number-fields', LAYA, [USED], [{ cause: 'load', evidence: { status: 1000, exitCode: 1.5, text: TEXT, crash: 'true' } }]);
add('evidence-status-504-rule', LAYA, [USED], [{ cause: 'load', evidence: { status: 504, exitCode: null, text: TEXT } }]);
add('evidence-exit-137-rule', LAYA, [USED], [{ cause: 'load', evidence: { status: '500', exitCode: 137, text: TEXT } }]);
add('evidence-crash-timeout-text', LAYA, [USED], [{ cause: 'load', evidence: { status: 500, exitCode: -1024, text: 'qa-engine timed out', crash: true } }]);
for (const cause of ['oom', 'load', 'timeout', 'time', 'recall', 'bogus', 'LOAD']) add(`cause-${cause}`, LAYA, [USED], [{ cause, evidence: ASK.evidence }]);
add('cause-missing', LAYA, [USED], [{ evidence: ASK.evidence }]);
add('cause-number', LAYA, [USED], [{ cause: 1, evidence: ASK.evidence }]);
add('cause-time-rule-text', LAYA, [USED], [{ cause: 'time', evidence: { status: 500, exitCode: 1, text: 'failed to load model qa.gguf' } }]);

// ── the stub decision service ───────────────────────────────────────────────────────────────
function responseBytes(r) {
  const body = r.bodyRepeat ? Buffer.from(r.bodyRepeat.text.repeat(r.bodyRepeat.count)) : r.bodyHex !== undefined ? Buffer.from(r.bodyHex, 'hex') : Buffer.from(r.body ?? '');
  const head = [`HTTP/1.1 ${r.status} QA`, ...(r.headers || []).map(([k, v]) => `${k}: ${v}`)];
  if (r.status !== 204) head.push(`Content-Length: ${body.length}`);
  head.push('Connection: close');
  return Buffer.concat([Buffer.from(head.join('\r\n') + '\r\n\r\n'), r.status === 204 ? Buffer.alloc(0) : body]);
}

function parseRequest(buf) {
  const end = buf.indexOf('\r\n\r\n');
  if (end < 0) return null;
  const lines = buf.subarray(0, end).toString('latin1').split('\r\n');
  const [method, target] = lines[0].split(' ');
  const headers = {};
  for (const line of lines.slice(1)) { const i = line.indexOf(':'); if (i > 0) headers[line.slice(0, i).trim().toLowerCase()] = line.slice(i + 1).trim(); }
  const length = Number(headers['content-length'] || 0);
  if (buf.length < end + 4 + length) return null;
  return { method, target, headers, body: buf.subarray(end + 4, end + 4 + length) };
}

/** The stub: requests in arrival order; replies scripted by arrival index. A single worker answers a
 *  request that arrives while another is unanswered with 503 (as the one-worker decision service). */
function startStub(stub) {
  return new Promise((resolve) => {
    const requests = [], sockets = new Set();
    let pending = 0, arrivals = 0;
    const server = net.createServer((socket) => {
      sockets.add(socket);
      socket.on('close', () => sockets.delete(socket));
      socket.on('error', () => {});
      let buf = Buffer.alloc(0), seen = false;
      socket.on('data', (chunk) => {
        if (seen) return;
        buf = Buffer.concat([buf, chunk]);
        const req = parseRequest(buf);
        if (!req) return;
        seen = true;
        const index = arrivals++;
        const busy = stub.singleWorker && pending > 0;
        const reply = busy ? BUSY : stub.replies[index] || json(500, { error: 'unscripted' });
        const body = req.body.toString('utf8');
        if (!Buffer.from(body, 'utf8').equals(req.body)) throw new Error('request body is not UTF-8');
        requests.push({ method: req.method, path: req.target, contentType: req.headers['content-type'] ?? null,
          authorization: req.headers.authorization ?? null, body, reply: busy ? 'busy' : index });
        pending++;
        let done = false;
        const finish = () => { if (!done) { done = true; pending--; } };
        if (reply.reset) { finish(); socket.destroy(); return; }
        if (reply.hang) { socket.on('close', finish); return; }
        setTimeout(() => { finish(); if (!socket.destroyed) socket.end(responseBytes(reply)); }, reply.delayMs || 0);
      });
    });
    server.listen(0, '127.0.0.1', () => {
      const port = server.address().port;
      const settled = async () => {
        const deadline = Date.now() + 12000;
        while ((sockets.size || pending) && Date.now() < deadline) await new Promise(r => setTimeout(r, 20));
        if (sockets.size || pending) throw new Error('stub did not settle');
      };
      const close = () => new Promise(r => server.close(r));
      if (stub.closed) server.close(() => resolve({ port, requests, settled: async () => {}, close: async () => {} }));
      else resolve({ port, requests, settled, close });
    });
  });
}

const withPort = (v, port) => typeof v === 'string' ? v.split(PORT).join(String(port)) : v;

async function run(sc) {
  const stub = await startStub(sc.stub);
  const env = Object.fromEntries(Object.entries(sc.env).map(([k, v]) => [k, withPort(v, stub.port)]));
  const advisor = createLoadAdvisor({ env, log: () => {} });
  const inputs = sc.inputs;
  let results;
  if (sc.calls === 'single') results = [await advisor.judge(inputs[0])];
  else if (sc.calls === 'sequential') { const a = await advisor.judge(inputs[0]); const b = await advisor.judge(inputs[1]); results = [a, b]; }
  else if (sc.calls === 'concurrent') { const pa = advisor.judge(inputs[0]); const b = await advisor.judge(inputs[1]); results = [await pa, b]; }
  else throw new Error('calls ' + sc.calls);
  await stub.settled();
  await stub.close();
  await new Promise(r => setTimeout(r, 50));
  return { ...sc, requests: stub.requests, results: results.map(r => JSON.stringify(r)) };
}

(async () => {
  const names = new Set();
  for (const sc of S) { if (names.has(sc.name)) throw new Error('duplicate ' + sc.name); names.add(sc.name); }
  const out = [];
  for (const sc of S) out.push(await run(sc));
  const sources = Object.fromEntries(SOURCES.map(f => [f, sha(f)]));
  const doc = { version: 1, sourceSha256: sources, scenarios: out };
  fs.writeFileSync(path.resolve(outArg), JSON.stringify(doc, null, 1) + '\n');
  fs.writeFileSync(path.join(__dirname, 'SOURCES.sha256'), SOURCES.map(f => `${sources[f]}  ${f}`).join('\n') + '\n');
  const advisors = {};
  for (const sc of out) for (const r of sc.results) { const a = JSON.parse(r)?.classification?.advisor ?? 'null'; advisors[a] = (advisors[a] || 0) + 1; }
  console.log(JSON.stringify({ scenarios: out.length, requests: out.reduce((n, s) => n + s.requests.length, 0), advisors }));
})().catch((e) => { console.error(e); process.exit(1); });
