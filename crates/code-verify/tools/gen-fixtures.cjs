'use strict';
// Writes tests/fixtures/node.v1.json: what noevia-core's own server/code-verify.cjs
// (`createCodeVerify`) and server/code-pipeline-verify.cjs (`createVerifyAdapter`) did against a
// scripted fake verifier: the outcome (status, report, error), every event it emitted and every log
// entry (in one ordered trace), the request lines the verifier received, whether the client hung up
// where the script waited for it, and what the checkout stub was asked. The Rust test
// (tests/replay.rs) plays the same fake verifier, from the same script, against the Rust crate and
// compares.
//
// Usage: node crates/code-verify/tools/gen-fixtures.cjs <noevia-core>/server > crates/code-verify/tests/fixtures/node.v1.json
// Synthetic data only. The fake verifier listens on loopback / a unix socket in a temp directory.
//
// A scenario (all fields filled in by `norm`, so the fixture holds the full definition):
//   options   { endpoint, timeoutMs, tailBytes?, connectRetryMs, clock? }  endpoint may use $DIR / $PORT
//   request   { taskId, repo, headSha, revision }
//   checkout  { throw: {message?, code?} } | { throwRaw: <value> }  (else returns the source)
//   server    'unix' | 'tcp' | 'none' | 'refused-tcp'
//   files     files to create in $DIR (a regular file in the middle of a socket path: ENOTDIR)
//   connections  one script per accepted connection, the last one repeats; steps:
//     { line }  read the request line (recorded)     { reply: obj }  write JSON + "\n"
//     { send: text }  { sendHex: [hex...], gapMs }  { sendBig: n }  raw writes
//     { wait: ms }  { close }  { expectClose } (records whether the client hung up)
//     { abort }  { abortAfter: ms }  the task's signal
//     String values may be { "$repeat": [text, n] } or { "$cat": [...] } (expanded by both sides).
//   cancelBefore  the signal is aborted before run()
//   adapter   { mode, ... } runs createVerifyAdapter over the verifier (see `buildVerify`)
//   nodeOnly  why the Rust replay skips it (cannot be expressed with the Rust types)

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const crypto = require('node:crypto');

const serverDir = path.resolve(process.argv[2] || '');
delete process.env.CODE_VERIFY_ENDPOINT;
const { createCodeVerify } = require(path.join(serverDir, 'code-verify.cjs'));
const { createVerifyAdapter } = require(path.join(serverDir, 'code-pipeline-verify.cjs'));

const SHA = '0123456789abcdef0123456789abcdef01234567';
const SHA64 = 'abcdef0123456789'.repeat(4);
const SOURCE = '/workspaces/qa-repo';
const NONCE = 'nonce-1'; // the counted nonceFn's first (and, per run, only) value
const rep = (s, n) => ({ $repeat: [s, n] });
const cat = (...parts) => ({ $cat: parts });

function expand(v) {
  if (Array.isArray(v)) return v.map(expand);
  if (v && typeof v === 'object') {
    if ('$repeat' in v) return v.$repeat[0].repeat(v.$repeat[1]);
    if ('$cat' in v) return v.$cat.map(expand).join('');
    return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, expand(x)]));
  }
  return v;
}

// ---- scenario building blocks -------------------------------------------------------------------
const okAns = (o = {}) => ({ noevia: 'verify-result', ok: true, nonce: NONCE, headSha: SHA, exitCode: 0, signal: null,
  timedOut: false, tail: 'all good\n', totalBytes: 9, truncated: false, ...o });
const once = (reply) => [[{ line: true }, { reply }, { expectClose: true }]];
const answered = (o) => once(okAns(o));
const raw = (text) => [[{ line: true }, { send: text }, { expectClose: true }]];
const S = {};
const bad = (name, request, extra = {}) => { S[name] = { request, connections: [], ...extra }; };

// input checks (before anything is sent)
S['no-endpoint'] = { options: { endpoint: null }, server: 'none' };
S['empty-endpoint'] = { options: { endpoint: '' }, server: 'none' };
bad('bad-repo-empty', { repo: '' });
bad('bad-repo-space', { repo: 'qa repo' });
bad('bad-repo-slash', { repo: 'qa/repo' });
bad('bad-repo-65', { repo: 'a'.repeat(65) });
bad('bad-repo-newline', { repo: 'qa-repo\n' });
bad('bad-repo-unicode', { repo: 'qa-répo' });
bad('bad-sha-39', { headSha: SHA.slice(1) });
bad('bad-sha-41', { headSha: SHA + '0' });
bad('bad-sha-uppercase', { headSha: SHA.toUpperCase() });
bad('bad-sha-nonhex', { headSha: 'g'.repeat(40) });
bad('bad-sha-empty', { headSha: '' });
bad('bad-sha-63', { headSha: SHA64.slice(1) });
bad('bad-revision-1.5', { revision: 1.5 });
bad('bad-revision-minus-1', { revision: -1 });
bad('bad-revision-null', { revision: null });
bad('bad-repo-before-sha-before-revision', { repo: '', headSha: 'x', revision: -1 });
S['repo-64-chars'] = { request: { repo: 'r'.repeat(64) }, connections: answered() };
S['repo-dots-dashes'] = { request: { repo: 'qa.repo-1_X' }, connections: answered() };
S['sha-64'] = { request: { headSha: SHA64 }, connections: answered({ headSha: SHA64 }) };
S['revision-zero'] = { request: { revision: 0 }, connections: answered() };
S['revision-large'] = { request: { revision: 4294967296 }, connections: answered() };
S['revision-1e21'] = { request: { revision: 1e21 }, connections: answered() };

// checkout
S['checkout-throws-with-code'] = { checkout: { throw: { message: 'No such task', code: 'no_task' } } };
S['checkout-throws-no-code'] = { checkout: { throw: { message: 'boom' } } };
S['checkout-throws-no-message'] = { checkout: { throw: { code: 'forbidden' } } };
S['checkout-throws-empty'] = { checkout: { throw: { message: '', code: '' } } };
S['checkout-throws-plain-string'] = { checkout: { throwRaw: 'plain string' } };
S['checkout-throws-empty-object'] = { checkout: { throwRaw: {} } };

// passed / failed
S['passed'] = { connections: answered() };
S['passed-over-tcp'] = { server: 'tcp', options: { endpoint: '127.0.0.1:$PORT' }, connections: answered() };
S['failed-exit-1'] = { connections: answered({ exitCode: 1, tail: 'FAIL qa\n', totalBytes: 8 }) };
S['failed-exit-negative'] = { connections: answered({ exitCode: -1 }) };
S['exit-float'] = { connections: answered({ exitCode: 1.5 }) };
S['exit-string'] = { connections: answered({ exitCode: '0' }) };
S['exit-missing'] = { connections: answered({ exitCode: undefined }) };
S['exit-null'] = { connections: answered({ exitCode: null }) };
S['exit-huge'] = { connections: answered({ exitCode: 1e300 }) };
S['exit-minus-zero'] = { connections: [[{ line: true }, { send: `{"noevia":"verify-result","ok":true,"nonce":"${NONCE}","headSha":"${SHA}","exitCode":-0,"totalBytes":-0}\n` }, { expectClose: true }]] };
S['exit-exponent'] = { connections: [[{ line: true }, { send: `{"noevia":"verify-result","ok":true,"nonce":"${NONCE}","headSha":"${SHA}","exitCode":0e0,"totalBytes":1e3}\n` }, { expectClose: true }]] };
S['signal-valid'] = { connections: answered({ exitCode: null, signal: 'SIGKILL' }) };
S['signal-valid-with-exit-0'] = { connections: answered({ exitCode: 0, signal: 'SIGTERM' }) };
S['signal-valid-10-chars'] = { connections: answered({ exitCode: null, signal: 'SIGABCDEFGHIJ' }) };
for (const [n, sig] of Object.entries({ lowercase: 'sigkill', bare: 'SIG', 'too-long': 'SIGABCDEFGHIJK', newline: 'SIGTERM\n', number: 123, 'dash': 'SIG-1', space: 'SIG TERM', unicode: 'SIGÉ' })) {
  S['signal-invalid-' + n] = { connections: answered({ signal: sig }) };
}
S['timed-out'] = { connections: answered({ exitCode: null, timedOut: true }) };
S['timed-out-with-exit-0'] = { connections: answered({ exitCode: 0, timedOut: true }) };
S['timed-out-string'] = { connections: answered({ timedOut: 'true' }) };

// tails
S['tail-controls'] = { connections: answered({ tail: 'a\r\nb\rc\u0000d\u001b[31me\u007ff\u0085g\th\n i\u000bj\u000ck\r\r\nl\u0001', totalBytes: undefined }) };
S['tail-cut-3-byte'] = { options: { tailBytes: 1024 }, connections: answered({ tail: rep('€', 400), totalBytes: undefined }) };
S['tail-cut-4-byte'] = { options: { tailBytes: 1024 }, connections: answered({ tail: cat(rep('😀', 300), 'xy'), totalBytes: undefined }) };
S['tail-cut-2-byte'] = { options: { tailBytes: 1024 }, connections: answered({ tail: cat(rep('é', 700), 'z'), totalBytes: undefined }) };
S['tail-lone-surrogates'] = { connections: answered({ tail: 'a\ud83db\ude00c😀d' }) };
S['tail-not-string'] = { connections: answered({ tail: 123 }) };
S['tail-null'] = { connections: answered({ tail: null }) };
S['tail-missing'] = { connections: answered({ tail: undefined, totalBytes: undefined }) };
S['tail-over-default-cap-no-total'] = { connections: answered({ tail: rep('y', 20000), totalBytes: undefined }) };
S['tail-1-5-mib'] = { connections: answered({ tail: rep('y', 1500000), totalBytes: 1500000 }) };
S['tail-bytes-0-default'] = { options: { tailBytes: 0 }, connections: answered({ tail: rep('z', 300000), totalBytes: undefined }) };
S['tail-bytes-10-min'] = { options: { tailBytes: 10 }, connections: answered({ tail: rep('z', 300000), totalBytes: undefined }) };
S['tail-bytes-minus-5-min'] = { options: { tailBytes: -5 }, connections: answered({ tail: rep('z', 3000), totalBytes: undefined }) };
S['tail-bytes-1e9-max'] = { options: { tailBytes: 1e9 }, connections: answered({ tail: rep('z', 300000), totalBytes: undefined }) };
S['tail-bytes-fraction'] = { options: { tailBytes: 2000.9 }, connections: answered({ tail: rep('z', 3000), totalBytes: undefined }) };

// totals
S['total-larger-than-tail'] = { connections: answered({ tail: 'abc', totalBytes: 5000, truncated: false }) };
S['total-equal-to-tail'] = { connections: answered({ tail: 'abc', totalBytes: 3 }) };
S['truncated-flag'] = { connections: answered({ tail: 'abc', totalBytes: 3, truncated: true }) };
S['truncated-flag-string'] = { connections: answered({ tail: 'abc', totalBytes: undefined, truncated: 'true' }) };
S['total-negative'] = { connections: answered({ totalBytes: -1 }) };
S['total-fraction'] = { connections: answered({ totalBytes: 1.5 }) };
S['total-string'] = { connections: answered({ totalBytes: '12' }) };
S['total-unsafe'] = { connections: [[{ line: true }, { send: `{"noevia":"verify-result","ok":true,"nonce":"${NONCE}","headSha":"${SHA}","exitCode":0,"totalBytes":9007199254740992}\n` }, { expectClose: true }]] };
S['total-max-safe'] = { connections: [[{ line: true }, { send: `{"noevia":"verify-result","ok":true,"nonce":"${NONCE}","headSha":"${SHA}","exitCode":0,"totalBytes":9007199254740991}\n` }, { expectClose: true }]] };

// answer checks
S['wrong-nonce'] = { connections: answered({ nonce: 'nonce-2' }) };
S['missing-nonce'] = { connections: answered({ nonce: undefined }) };
S['wrong-head-sha'] = { connections: answered({ headSha: 'f'.repeat(40) }) };
S['missing-head-sha'] = { connections: answered({ headSha: undefined }) };
S['not-a-verify-result'] = { connections: answered({ noevia: 'other' }) };
S['missing-noevia'] = { connections: answered({ noevia: undefined }) };
for (const [n, body] of Object.entries({ null: 'null', number: '5', string: '"verify-result"', array: '[]', zero: '0', false: 'false', empty_object: '{}' })) {
  S['answer-is-' + n] = { connections: raw(body + '\n') };
}
S['nonce-checked-before-ok'] = { connections: answered({ ok: false, error: 'boom', nonce: 'nonce-2' }) };
S['head-not-checked-when-refused'] = { connections: answered({ ok: false, error: 'boom', headSha: 'f'.repeat(40) }) };

// refusals
S['refused-not-configured'] = { connections: answered({ ok: false, error: 'not_configured', message: 'No CODE_VERIFY set.' }) };
S['refused-other'] = { connections: answered({ ok: false, error: 'bad_source', message: 'The source is not a git tree.' }) };
S['refused-long-code'] = { connections: answered({ ok: false, error: 'c'.repeat(60) }) };
S['refused-code-emoji-at-cut'] = { connections: answered({ ok: false, error: 'x'.repeat(39) + '😀tail' }) };
S['refused-code-not-string'] = { connections: answered({ ok: false, error: 123, message: 'x' }) };
S['refused-code-missing'] = { connections: answered({ ok: false }) };
S['refused-ok-not-boolean'] = { connections: answered({ ok: 'yes', error: 'nope' }) };
S['refused-ok-missing'] = { connections: answered({ ok: undefined }) };
S['refused-message-controls'] = { connections: answered({ ok: false, error: 'bad', message: cat('a\r\nb\u001b[0m\tc\u0000', rep('m', 700), 'END') }) };
S['refused-message-multibyte-cut'] = { connections: answered({ ok: false, error: 'bad', message: cat(rep('€', 300), 'q') }) };
S['refused-message-not-string'] = { connections: answered({ ok: false, error: 'bad', message: 42 }) };
S['refused-message-empty'] = { connections: answered({ ok: false, error: 'bad', message: '' }) };
S['refused-message-lone-surrogate'] = { connections: answered({ ok: false, error: 'bad', message: 'a\ud83db' }) };
S['refused-busy-after-retries'] = { options: { connectRetryMs: 375 }, connections: [[{ line: true }, { reply: okAns({ ok: false, error: 'busy', message: 'Another run is in progress.' }) }, { expectClose: true }]] };
S['refused-busy-no-retry-window'] = { options: { connectRetryMs: 0 }, connections: once(okAns({ ok: false, error: 'busy' })) };
S['busy-then-success'] = { options: { connectRetryMs: 5000 }, connections: [
  [{ line: true }, { reply: okAns({ ok: false, error: 'busy' }) }, { expectClose: true }],
  [{ line: true }, { reply: okAns({ tail: 'second try\n', totalBytes: 11 }) }, { expectClose: true }]] };
S['busy-answer-ignores-wrong-nonce-while-retrying'] = { options: { connectRetryMs: 375 }, connections: [[{ line: true }, { reply: { ok: false, error: 'busy', nonce: 'whatever' } }, { expectClose: true }]] };
S['busy-without-noevia-retries-then-refuses'] = { options: { connectRetryMs: 375 }, connections: [[{ line: true }, { reply: { ok: false, error: 'busy', nonce: NONCE } }, { expectClose: true }]] };

// unreadable answers
S['answer-not-json'] = { connections: raw('this is not json\n') };
S['answer-empty-line'] = { connections: raw('\n') };
S['answer-truncated-json'] = { connections: raw('{"noevia":\n') };
S['answer-trailing-garbage-in-line'] = { connections: raw(JSON.stringify(okAns()) + ' x\n') };
S['answer-crlf'] = { connections: raw(JSON.stringify(okAns()) + '\r\n') };
S['answer-leading-spaces'] = { connections: raw('   ' + JSON.stringify(okAns()) + '\n') };
S['answer-extra-after-newline'] = { connections: raw(JSON.stringify(okAns()) + '\n{"junk":1}\nmore') };
S['answer-two-keys-repeated'] = { connections: raw(`{"noevia":"x","noevia":"verify-result","ok":true,"nonce":"${NONCE}","headSha":"${SHA}","exitCode":0}\n`) };
S['answer-escaped-unicode'] = { connections: raw(`{"noevia":"verify-result","ok":true,"nonce":"${NONCE}","headSha":"${SHA}","exitCode":0,"tail":"\\u00e9\\ud83d\\ude00\\u0000x"}\n`) };
{
  const text = Buffer.from(JSON.stringify(okAns({ tail: 'a\u0001QQb', totalBytes: undefined })) + '\n');
  const hex = (b) => b.toString('hex');
  const at = text.indexOf('QQ');
  S['answer-invalid-utf8'] = { connections: [[{ line: true }, { sendHex: [hex(Buffer.concat([text.subarray(0, at), Buffer.from([0xff, 0xc3]), text.subarray(at + 2)]))] }, { expectClose: true }]] };
  const euro = JSON.stringify(okAns({ tail: 'a€b€c', totalBytes: undefined })) + '\n';
  const eb = Buffer.from(euro), e1 = eb.indexOf(Buffer.from('€'));
  const e2 = eb.indexOf(Buffer.from('€'), e1 + 3);
  S['answer-multibyte-split-across-chunks'] = { connections: [[{ line: true },
    { sendHex: [hex(eb.subarray(0, e1 + 1)), hex(eb.subarray(e1 + 1, e1 + 2)), hex(eb.subarray(e1 + 2, e2 + 2)), hex(eb.subarray(e2 + 2))], gapMs: 40 }, { expectClose: true }]] };
  const plain = Buffer.from(JSON.stringify(okAns()) + '\n');
  S['answer-in-three-chunks'] = { connections: [[{ line: true },
    { sendHex: [hex(plain.subarray(0, 10)), hex(plain.subarray(10, 60)), hex(plain.subarray(60))], gapMs: 40 }, { expectClose: true }]] };
  S['answer-partial-multibyte-then-close'] = { options: { connectRetryMs: 0 }, connections: [[{ line: true }, { sendHex: ['e2'] }, { close: true }]] };
}
S['answer-over-2-mib-no-newline'] = { connections: [[{ line: true }, { sendBig: 2 * 1024 * 1024 + 1 }, { expectClose: true }]] };
S['answer-exactly-2-mib-then-close'] = { options: { connectRetryMs: 0 }, connections: [[{ line: true }, { sendBig: 2 * 1024 * 1024 }, { close: true }]] };
S['answer-partial-then-close'] = { options: { connectRetryMs: 0 }, connections: [[{ line: true }, { send: '{"noevia":"verify-res' }, { close: true }]] };

// not listening
// Accepted and dropped without reading the request (the request is unread when it closes: a reset).
// A close before the request even went out is a `write EPIPE` race in Node (unreachable) that a
// second thread cannot reproduce, so the script lets the request arrive first.
S['not-listening-immediate-close'] = { options: { connectRetryMs: 375 }, connections: [[{ wait: 60 }, { close: true }]] };
S['not-listening-close-after-request'] = { options: { connectRetryMs: 375 }, connections: [[{ line: true }, { close: true }]] };
S['not-listening-no-retry-window'] = { options: { connectRetryMs: 0 }, connections: [[{ line: true }, { close: true }]] };
S['not-listening-then-success'] = { options: { connectRetryMs: 5000 }, connections: [
  [{ line: true }, { close: true }],
  [{ line: true }, { reply: okAns() }, { expectClose: true }]] };
S['socket-file-missing'] = { server: 'none', options: { endpoint: 'unix:$DIR/missing.sock', connectRetryMs: 375 } };
S['tcp-connection-refused'] = { server: 'refused-tcp', options: { endpoint: '127.0.0.1:$PORT', connectRetryMs: 375 } };
S['tcp-connection-refused-no-window'] = { server: 'refused-tcp', options: { endpoint: '127.0.0.1:$PORT', connectRetryMs: 0 } };
S['socket-path-through-a-file'] = { server: 'none', files: ['afile'], options: { endpoint: 'unix:$DIR/afile/sock', connectRetryMs: 375 } };
S['socket-path-too-long'] = { server: 'none', options: { endpoint: 'unix:/' + 'a'.repeat(200), connectRetryMs: 375 } };

// timeouts and slow closes
S['never-answers-timeout'] = { options: { timeoutMs: 300 }, connections: [[{ line: true }, { expectClose: true }]] };
S['answers-too-late'] = { options: { timeoutMs: 300 }, connections: [[{ line: true }, { wait: 600 }, { reply: okAns() }, { expectClose: true }]] };
S['timeout-is-not-retried'] = { options: { timeoutMs: 200, connectRetryMs: 5000 }, connections: [[{ line: true }, { expectClose: true }]] };
S['accepts-waits-then-closes-unreachable'] = { options: { timeoutMs: 4000 }, connections: [[{ line: true }, { wait: 1200 }, { close: true }]] };

// endpoints
for (const [n, ep] of Object.entries({
  'relative-unix': 'unix:relative/v.sock', 'unix-empty': 'unix:', 'no-colon': 'nocolon', 'empty-port': 'host:', 'empty-host': ':80',
  'two-colons': 'a:b:80', 'port-70000': '127.0.0.1:70000', 'port-huge': '127.0.0.1:99999999999999999999999',
  'v6-port-70000': '[::1]:70000', 'port-not-digits': 'host:80a', 'unix-uppercase-scheme': 'UNIX:/tmp/x', 'port-65536': 'localhost:65536',
})) S['endpoint-' + n] = { server: 'none', options: { endpoint: ep } };

// cancel
S['cancel-before'] = { cancelBefore: true, connections: [] };
S['cancel-during'] = { connections: [[{ line: true }, { abort: true }, { expectClose: true }]] };
S['cancel-during-retry-wait'] = { options: { connectRetryMs: 5000 }, connections: [[{ line: true }, { close: true }, { abortAfter: 100 }]] };
S['cancel-before-bad-endpoint'] = { cancelBefore: true, server: 'none', options: { endpoint: 'unix:relative' } };
S['cancel-before-checkout-fails'] = { cancelBefore: true, checkout: { throw: { message: 'late', code: 'x' } }, connections: [] };

// clock
S['clock-backwards'] = { options: { clock: [100, 50] }, connections: answered() };
S['clock-slow'] = { options: { clock: [1000, 4321] }, connections: answered() };

// adapter over each class
const A = (adapter, base = {}) => ({ adapter, ...base });
S['adapter-no-verifier'] = A({ mode: 'none' }, { server: 'none' });
S['adapter-unavailable-no-endpoint'] = A({ mode: 'real' }, { options: { endpoint: null }, server: 'none' });
S['adapter-error-bad-sha'] = A({ mode: 'real' }, { request: { headSha: 'nope' }, connections: [] });
S['adapter-error-long-code-and-message'] = A({ mode: 'real' }, { connections: answered({ ok: false, error: 'e'.repeat(60), message: 'm'.repeat(700) }) });
S['adapter-unavailable-busy'] = A({ mode: 'real' }, { options: { connectRetryMs: 375 }, connections: [[{ line: true }, { reply: okAns({ ok: false, error: 'busy', message: 'b'.repeat(400) }) }, { expectClose: true }]] });
S['adapter-unavailable-not-configured'] = A({ mode: 'real' }, { connections: answered({ ok: false, error: 'not_configured' }) });
S['adapter-failed'] = A({ mode: 'real' }, { connections: answered({ exitCode: 1 }) });
S['adapter-failed-timed-out'] = A({ mode: 'real' }, { connections: answered({ exitCode: 0, timedOut: true }) });
S['adapter-passed'] = A({ mode: 'real' }, { connections: answered() });
S['adapter-unmeasured-report'] = A({ mode: 'unmeasured' }, { connections: answered() });
S['adapter-report-for-another-task'] = A({ mode: 'rebound-task' }, { connections: answered() });
S['adapter-report-for-another-revision'] = A({ mode: 'rebound-revision' }, { connections: answered() });
S['adapter-stub-passed-without-report'] = A({ mode: 'stub', stub: { status: 'passed', error: null } }, { server: 'none' });
S['adapter-stub-failed-without-report'] = A({ mode: 'stub', stub: { status: 'failed', error: null } }, { server: 'none' });
S['adapter-stub-unavailable-no-reason'] = A({ mode: 'stub', stub: { status: 'unavailable', error: { code: 'busy', message: '' } } }, { server: 'none' });
S['adapter-stub-unavailable-long-reason'] = A({ mode: 'stub', stub: { status: 'unavailable', error: { code: 'x', message: 'r'.repeat(500) } } }, { server: 'none' });
S['adapter-stub-error-no-code'] = A({ mode: 'stub', stub: { status: 'error', error: { code: '', message: '' } } }, { server: 'none' });
S['adapter-stub-error-long'] = A({ mode: 'stub', stub: { status: 'error', error: { code: 'c'.repeat(50), message: 'm'.repeat(301) } } }, { server: 'none' });
S['adapter-stub-error-emoji-at-cut'] = A({ mode: 'stub', stub: { status: 'error', error: { code: 'c'.repeat(39) + '😀', message: 'm'.repeat(299) + '😀' } } }, { server: 'none' });
S['adapter-stub-error-without-error-object'] = A({ mode: 'stub', stub: { status: 'error', error: null } }, { server: 'none' });
S['adapter-stub-unavailable-without-error-object'] = A({ mode: 'stub', stub: { status: 'unavailable', error: null } }, { server: 'none' });
S['adapter-verifier-throws'] = A({ mode: 'threw' }, { server: 'none' });
S['adapter-available-throws'] = A({ mode: 'available-throws' }, { server: 'none' });
S['adapter-unknown-status'] = A({ mode: 'stub', stub: { status: 'weird', error: null } }, { server: 'none', nodeOnly: 'an outcome with a status outside the four cannot be written with the Rust Outcome type' });
S['adapter-copied-report'] = A({ mode: 'copied-report' }, { connections: answered(), nodeOnly: 'a copy of a report ({...report}, a different object in Node) cannot be built in Rust: Report has no public constructor' });

// ---- defaults ---------------------------------------------------------------------------------
function norm(sc) {
  const out = {};
  if (sc.adapter) out.adapter = sc.adapter;
  if (sc.nodeOnly) out.nodeOnly = sc.nodeOnly;
  const server = sc.server || 'unix';
  out.server = server;
  const options = { timeoutMs: 3000, connectRetryMs: 0, ...(sc.options || {}) };
  if (!('endpoint' in options)) options.endpoint = server === 'unix' ? 'unix:$DIR/v.sock' : server === 'tcp' ? '127.0.0.1:$PORT' : null;
  out.options = options;
  out.request = { taskId: 'qa-task', repo: 'qa-repo', headSha: SHA, revision: 1, ...(sc.request || {}) };
  if (sc.checkout) out.checkout = sc.checkout;
  if (sc.files) out.files = sc.files;
  out.connections = sc.connections || [[{ close: true }]];
  if (sc.cancelBefore) out.cancelBefore = true;
  return JSON.parse(JSON.stringify(out));
}

// ---- the fake verifier ----------------------------------------------------------------------------
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
function reader(socket) {
  let data = Buffer.alloc(0), ended = false;
  const waiters = [];
  const wake = () => waiters.splice(0).forEach((f) => f());
  socket.on('data', (d) => { data = Buffer.concat([data, d]); wake(); });
  const end = () => { ended = true; wake(); };
  socket.on('end', end); socket.on('close', end); socket.on('error', end);
  const until = (pred, ms) => new Promise((resolve) => {
    const t = setTimeout(() => resolve(false), ms);
    const check = () => { if (pred()) { clearTimeout(t); resolve(true); } else waiters.push(check); };
    check();
  });
  return {
    async line(ms) {
      await until(() => data.includes(10) || ended, ms);
      const i = data.indexOf(10);
      if (i === -1) return null;
      const l = data.subarray(0, i).toString('utf8');
      data = data.subarray(i + 1);
      return l;
    },
    closed: (ms) => until(() => ended, ms),
  };
}

async function script(socket, steps, ctx) {
  const rd = reader(socket);
  socket.on('error', () => {});
  const write = (b) => { try { socket.write(b); } catch { /* client gone */ } };
  for (const step of steps) {
    if (step.line) { const l = await rd.line(10000); if (l !== null) ctx.lines.push(l); }
    else if (step.reply !== undefined) write(JSON.stringify(expand(step.reply)) + '\n');
    else if (step.send !== undefined) write(expand(step.send));
    else if (step.sendHex) { for (const h of step.sendHex) { write(Buffer.from(h, 'hex')); if (step.gapMs) await sleep(step.gapMs); } }
    else if (step.sendBig) write(Buffer.alloc(step.sendBig, 0x78));
    else if (step.wait) await sleep(step.wait);
    else if (step.close) socket.destroy();
    else if (step.expectClose) ctx.closes.push(await rd.closed(10000));
    else if (step.abort) ctx.abort();
    else if (step.abortAfter) setTimeout(ctx.abort, step.abortAfter);
  }
}

function makeClock(spec) {
  if (Array.isArray(spec)) { let i = 0; return () => spec[Math.min(i++, spec.length - 1)]; }
  let t = 1_700_000_000_000;
  return () => (t += 7);
}

function buildVerify(sc, verify) {
  const a = sc.adapter;
  if (a.mode === 'none') return createVerifyAdapter(null);
  const base = { available: () => verify.available(), run: (args) => verify.run(args), isMeasured: (r, e) => verify.isMeasured(r, e) };
  switch (a.mode) {
    case 'real': return createVerifyAdapter(verify);
    case 'unmeasured': return createVerifyAdapter({ ...base, isMeasured: () => false });
    case 'rebound-task': return createVerifyAdapter({ ...base, run: (args) => verify.run({ ...args, taskId: 'qa-other-task' }) });
    case 'rebound-revision': return createVerifyAdapter({ ...base, run: (args) => verify.run({ ...args, revision: args.revision + 1 }) });
    case 'copied-report': return createVerifyAdapter({ ...base, run: async (args) => { const o = await verify.run(args); return { ...o, report: { ...o.report } }; } });
    case 'stub': return createVerifyAdapter({ available: () => true, isMeasured: (r) => !!r, run: async () => ({ status: a.stub.status, report: null, error: a.stub.error }) });
    case 'threw': return createVerifyAdapter({ available: () => true, isMeasured: () => true, run: async () => { throw Error('qa boom'); } });
    case 'available-throws': return createVerifyAdapter({ available: () => { throw Error('qa'); }, isMeasured: () => true, run: async () => ({}) });
    default: throw Error('mode ' + a.mode);
  }
}

async function runScenario(name, sc) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-vfy-'));
  for (const f of sc.files || []) fs.writeFileSync(path.join(dir, f), 'qa');
  const ctx = { lines: [], closes: [], abort: () => controller.abort() };
  const controller = new AbortController();
  const sockets = new Set(), running = [];
  let server = null, port = 0, count = 0;
  const handler = (socket) => {
    sockets.add(socket);
    const steps = sc.connections[Math.min(count++, sc.connections.length - 1)];
    running.push(script(socket, steps, ctx).catch(() => {}));
  };
  if (sc.server === 'unix' || sc.server === 'tcp') {
    server = net.createServer(handler);
    await new Promise((r) => (sc.server === 'unix' ? server.listen(path.join(dir, 'v.sock'), r) : server.listen(0, '127.0.0.1', r)));
    if (sc.server === 'tcp') port = server.address().port;
  } else if (sc.server === 'refused-tcp') {
    const s = net.createServer();
    await new Promise((r) => s.listen(0, '127.0.0.1', r));
    port = s.address().port;
    await new Promise((r) => s.close(r));
  }
  const o = sc.options;
  const endpoint = o.endpoint === null ? null : o.endpoint.split('$DIR').join(dir).split('$PORT').join(String(port));
  const trace = [];
  let nonces = 0;
  const verify = createCodeVerify({
    endpoint, timeoutMs: o.timeoutMs, connectRetryMs: o.connectRetryMs, now: makeClock(o.clock),
    nonceFn: () => 'nonce-' + (++nonces),
    ...(o.tailBytes !== undefined ? { tailBytes: o.tailBytes } : {}),
    log: (entry) => trace.push({ log: JSON.parse(JSON.stringify(entry)) }),
  });
  const checkoutCalls = [];
  const workspaces = {
    verifyCheckout: (taskId, headSha) => {
      checkoutCalls.push([taskId, headSha]);
      if (sc.checkout?.throw) { const e = Error(sc.checkout.throw.message); if ('code' in sc.checkout.throw) e.code = sc.checkout.throw.code; throw e; }
      if (sc.checkout && 'throwRaw' in sc.checkout) throw sc.checkout.throwRaw;
      return { source: SOURCE };
    },
  };
  const emit = (type, data) => trace.push({ event: { type, data: JSON.parse(JSON.stringify(data)) } });
  if (sc.cancelBefore) controller.abort();
  const target = sc.adapter ? buildVerify(sc, verify) : verify;
  const available = target.available();
  const outcome = await target.run({ workspaces, ...sc.request, emit, signal: controller.signal });
  await Promise.race([Promise.all(running), sleep(15000)]);
  await sleep(30);
  for (const s of sockets) s.destroy();
  if (server) await new Promise((r) => server.close(r));
  fs.rmSync(dir, { recursive: true, force: true });
  const record = { available, outcome: JSON.parse(JSON.stringify(outcome)), trace, lines: ctx.lines, closes: ctx.closes, checkoutCalls };
  // The temp directory is part of some messages: normalised.
  return JSON.parse(JSON.stringify(record).split(dir).join('<DIR>'));
}

(async () => {
  const keepAlive = setInterval(() => {}, 1000);
  const pin = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(serverDir, f))).digest('hex');
  const only = process.env.QA_ONLY ? new RegExp(process.env.QA_ONLY) : null;
  const out = {};
  for (const [name, def] of Object.entries(S)) {
    if (only && !only.test(name)) continue;
    process.stderr.write(`scenario ${name}\n`);
    const sc = norm(def);
    out[name] = { ...sc, expect: await runScenario(name, sc) };
  }
  process.stdout.write(JSON.stringify({ generator: 'crates/code-verify/tools/gen-fixtures.cjs',
    sources: { 'code-verify.cjs': pin('code-verify.cjs'), 'code-pipeline-verify.cjs': pin('code-pipeline-verify.cjs'), 'code-acp.cjs': pin('code-acp.cjs') },
    source: SOURCE, scenarios: out }, null, 1) + '\n');
  clearInterval(keepAlive);
})();
