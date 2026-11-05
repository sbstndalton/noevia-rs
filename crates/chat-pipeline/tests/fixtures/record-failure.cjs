#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own chat.cjs providerFailureText (its lines, read from chat.cjs) on
// synthetic failed replies (a real fetch Response over the bytes) and records the text it shows. Synthetic data only.
//   node record-failure.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const source = fs.readFileSync(path.join(core, 'server', 'chat.cjs'), 'utf8');
const at = source.indexOf('async function providerFailureText(');
const stop = source.indexOf('\n}\n', at);
if (at < 0 || stop < 0) throw new Error('providerFailureText moved');
const near = (m) => require(m.startsWith('./') ? path.join(core, 'server', m) : m);
const { readCappedText } = near('./http.cjs');
// eslint-disable-next-line no-new-func
const providerFailureText = new Function('require', 'readCappedText', 'PROVIDER_ERROR_BODY_CAP', `${source.slice(at, stop + 2)}\nreturn providerFailureText;`)(near, readCappedText, 64 * 1024);
const big = (n, c = 'x') => c.repeat(n);
const BODIES = [
  '', 'oops', '{"error":{"message":"context size has been exceeded","code":400}}',
  '{"error":{"message":"Unable to generate parser for this template. Conversation roles must alternate user/assistant"}}',
  '{"error":{"type":"gate_error","message":"The engine queue is full; try again in a moment."}}',
  '{"error":{"type":"gate_error","message":""}}', '{"error":{"type":"other","message":"no"}}',
  '{"error":{"message":"You have hit your usage limit. It resets at 5 pm."}}', '{"error":{"message":7}}', '{"error":null}', '{"error":{}}',
  '{"error":{"message":"' + big(400, 'é') + '"}}', 'connect ECONNREFUSED 10.0.0.5:8080 at http://10.0.0.5/v1 Bearer sk-abcdefghijklmnopqrstu',
  'model is loading', '<html>502 Bad Gateway</html>', 'invalid request: messages[3].role', big(70000, 'ab😀'), big(65535) + '😀tail', big(65536),
  '{"error":{"type":"gate_error","message":{"toString":1}}}',
];
const RAW = [Buffer.from([0x7b, 0xff, 0xfe, 0x7d]), Buffer.concat([Buffer.from(big(65535)), Buffer.from([0xe2, 0x82])]), Buffer.concat([Buffer.from(big(65534)), Buffer.from('€'), Buffer.from('more')])];
const STATUSES = [undefined, 0, 400, 404, 413, 422, 429, 500, 502, 503, 504, 200];
const HEADERS = [{}, { 'x-noevia-gate-refusal': '1' }, { 'x-noevia-provider-message': '1' }, { 'x-noevia-provider-message': '0' }, { 'x-noevia-gate-refusal': '1', 'x-noevia-provider-message': '1' }];
(async () => {
  const bodies = [...BODIES.map((b) => Buffer.from(b, 'utf8')), ...RAW];
  const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), bodies: bodies.map((b) => b.toString('base64')), cases: [] };
  for (const [index, bytes] of bodies.entries()) for (const status of STATUSES) for (const headers of HEADERS) for (const chatgpt of [false, true]) for (const partial of [false, true]) {
    if (bytes.length > 1000 && (chatgpt || headers['x-noevia-provider-message'] === '0' || ![undefined, 400, 500, 502].includes(status))) continue;
    let response;
    if (Number.isInteger(status) && status >= 200 && status <= 599) response = new Response(bytes, { status, headers });
    else if (bytes.equals(Buffer.from(bytes.toString('utf8'), 'utf8'))) response = { status, headers: new Headers(headers), text: async () => bytes.toString('utf8') };
    else continue;
    const text = await providerFailureText(response, chatgpt, partial);
    out.cases.push({ status: status === undefined ? null : status, body: index, gate: !!headers['x-noevia-gate-refusal'], providerMessage: headers['x-noevia-provider-message'] === '1', chatgpt, partial, text });
  }
  fs.writeFileSync(process.argv[3], JSON.stringify(out));
  console.log(`recorded ${out.cases.length} failure cases`);
})();
