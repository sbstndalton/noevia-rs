#!/usr/bin/env node
'use strict';
// Recorded usage writes (track R12) for crates/server-usage `write`.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/server-usage/tests/record_write.cjs > crates/server-usage/tests/fixtures/node-usage-write.v1.json
//   ... node crates/server-usage/tests/record_write.cjs --check crates/server-usage/tests/fixtures/node-usage-write.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every expectation comes from noevia-core's own code: usage.cjs `recordUsage` and `recordToolUse` (with
// workspace.cjs `atomicJson`), called on a workspace stand-in whose usagePath is a fresh temporary file. The clock
// is fixed per call (`Date` answers the case's instant; `TZ` is the case's zone), so a recording is reproducible.
// Recorded: the file's bytes after each call (null: no file). Cases marked `difference` hold hand-edited values the
// port leaves alone instead of adding to (write.rs header). Inputs are synthetic and QA-named. Each source file's
// sha256 is recorded. stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
const warnings = [];
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => warnings.push(a.map(String).join(' '));
const READ_FILES = ['server/usage.cjs', 'server/workspace.cjs'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex');

const RealDate = Date;
let NOW = 0;
global.Date = class extends RealDate {
  constructor(...args) { super(...(args.length ? args : [NOW])); }
  static now() { return NOW; }
};
const { recordUsage, recordToolUse } = require(path.join(root, 'server/usage.cjs'));

const T = (iso) => new RealDate(iso).getTime();
const NY = 'America/New_York';
const at = (iso, tz = NY) => ({ now: T(iso), tz });
const u = (model, promptTokens, completionTokens, clock = at('2026-11-14T15:30:00Z')) => ({ op: 'usage', model, usage: { promptTokens, completionTokens }, ...clock });
const tool = (name, clock = at('2026-11-14T15:30:00Z')) => ({ op: 'tool', name, ...clock });
const day = (fields) => JSON.stringify({ input: 1, output: 2, replies: 1, models: { 'qa-model': { input: 1, output: 2, replies: 1 } }, hours: { 10: 1 }, ...fields });

const cases = [
  ['first-reply', null, [u('qa-model', 10, 5)]],
  ['two-models-same-day', null, [u('qa-model', 10, 5), u('qa-other', 3, 0), u('qa-model', 1, 1)]],
  ['tools', null, [tool('qa_search'), tool('qa_search'), tool('qa_fetch'), u('qa-model', 2, 2), tool('qa_search')]],
  ['tool-only-day', null, [tool('qa_search')]],
  ['existing-day', `{"days":{"2026-11-14":${day({})}}}`, [u('qa-model', 4, 4), tool('qa_tool')]],
  ['extra-top-level-kept', '{"qaNote":"keep","days":{},"z":[1]}', [u('qa-model', 1, 0)]],
  ['days-null-reset', '{"qaNote":"dropped","days":null}', [u('qa-model', 1, 0)]],
  ['days-number-reset', '{"days":5}', [u('qa-model', 1, 0)]],
  ['top-array', '[1,2]', [u('qa-model', 1, 0)]],
  ['top-null', 'null', [u('qa-model', 1, 0)]],
  ['malformed', '{"days":', [u('qa-model', 1, 0)]],
  ['bom', '﻿{"days":{"2026-11-13":{"input":1}}}', [u('qa-model', 1, 0)]],
  ['retention', '{"days":{"2025-11-13":{"input":1},"2025-11-14":{"input":2},"2025-11-15":{"input":3},"abc":{},"1":{},"2026-01-01":{}}}', [u('qa-model', 1, 0)]],
  ['retention-not-on-tool', '{"days":{"2020-01-01":{"input":1}}}', [tool('qa_tool')]],
  ['string-tokens', null, [u('qa-model', '12', ' 3 ')]],
  ['hex-and-exp-tokens', null, [u('qa-model', '0x10', '1e2')]],
  ['bad-tokens-zero', null, [u('qa-model', 'abc', null)]],
  ['negative-tokens', null, [u('qa-model', -1, 5)]],
  ['infinite-tokens', null, [u('qa-model', 'Infinity', 1)]],
  ['zero-tokens', null, [u('qa-model', 0, 0)]],
  ['bool-tokens', null, [u('qa-model', true, false)]],
  ['fractional-tokens', null, [u('qa-model', 1.5, 0.25)]],
  ['no-usage', null, [{ op: 'usage', model: 'qa-model', usage: null, ...at('2026-11-14T15:30:00Z') }]],
  ['usage-number', null, [{ op: 'usage', model: 'qa-model', usage: 5, ...at('2026-11-14T15:30:00Z') }]],
  ['model-missing', null, [u(null, 1, 1), u('', 1, 1), u(0, 1, 1)]],
  ['model-number', null, [u(42, 1, 1)]],
  ['model-proto', null, [u('__proto__', 1, 1), u('constructor', 1, 1), u('2', 1, 1), u('10', 1, 1)]],
  ['model-unicode', null, [u('qa-mödel-😀', 1, 1)]],
  ['model-own-tostring', null, [u({ toString: 1 }, 1, 1)]],
  ['tool-names', null, [tool(5), tool(''), tool(null), tool('q'.repeat(100)), tool('a'.repeat(79) + '😀'), tool('__proto__'), tool('7')]],
  ['hours-ordering', null, [u('qa-model', 1, 1, at('2026-11-14T20:00:00Z')), u('qa-model', 1, 1, at('2026-11-14T14:00:00Z')), u('qa-model', 1, 1, at('2026-11-14T05:30:00Z'))]],
  ['hours-odd-values', `{"days":{"2026-11-14":${day({ hours: { 10: null, 9: '4', 11: 'x' } })}}}`, [u('qa-model', 1, 1, at('2026-11-14T15:00:00Z')), u('qa-model', 1, 1, at('2026-11-14T14:00:00Z')), u('qa-model', 1, 1, at('2026-11-14T16:00:00Z'))]],
  ['tools-odd-values', `{"days":{"2026-11-14":${day({ tools: { a: '2', b: null, c: true } })}}}`, [tool('a'), tool('b'), tool('c')]],
  ['local-midnight-new-york', null, [u('qa-model', 1, 1, at('2026-11-15T04:30:00Z')), u('qa-model', 1, 1, at('2026-11-15T05:30:00Z'))]],
  ['kolkata', null, [u('qa-model', 1, 1, at('2026-11-14T19:00:00Z', 'Asia/Kolkata'))]],
  ['utc', null, [u('qa-model', 1, 1, at('2026-11-14T23:59:59Z', 'UTC'))]],
  ['dst-spring-forward', null, [u('qa-model', 1, 1, at('2026-03-08T07:30:00Z')), u('qa-model', 1, 1, at('2026-03-08T06:30:00Z'))]],
  ['dst-cutoff', null, [u('qa-model', 1, 1, at('2027-03-09T04:30:00Z'))]],
  ['leap-day-cutoff', '{"days":{"2027-02-28":{},"2027-03-01":{},"2028-02-28":{}}}', [u('qa-model', 1, 1, at('2028-02-29T12:00:00Z', 'UTC'))]],
  ['days-list', '{"days":[{"input":1}]}', [u('qa-model', 1, 1)], 'unusual'],
  ['day-string-count', '{"days":{"2026-11-14":{"input":"5","output":0,"replies":0,"models":{}}}}', [u('qa-model', 1, 1)], 'unusual'],
  ['day-missing-replies', '{"days":{"2026-11-14":{"input":0,"output":0,"models":{}}}}', [u('qa-model', 1, 1)], 'unusual'],
  ['day-not-object', '{"days":{"2026-11-14":"x"}}', [u('qa-model', 1, 1)], 'unusual'],
  ['model-entry-not-object', '{"days":{"2026-11-14":{"input":0,"output":0,"replies":0,"models":{"qa-model":5}}}}', [u('qa-model', 1, 1)], 'unusual'],
  ['huge-number', '{"days":{"2026-11-13":{"input":1e999}}}', [u('qa-model', 1, 1)], 'unusual'],
];

function run([name, start, ops, difference]) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-qa-usage-'));
  const file = path.join(dir, 'usage.json');
  if (start !== null) fs.writeFileSync(file, start);
  const workspace = { usagePath: () => file, revoked: false, assertActive() {} };
  const after = [];
  for (const op of ops) {
    process.env.TZ = op.tz;
    NOW = op.now;
    warnings.length = 0;
    if (op.op === 'usage') recordUsage(workspace, op.model, op.usage);
    else recordToolUse(workspace, op.name);
    after.push({ file: fs.existsSync(file) ? fs.readFileSync(file, 'utf8') : null, warned: warnings.length > 0 });
  }
  fs.rmSync(dir, { recursive: true, force: true });
  return { name, start, ops, ...(difference ? { difference } : {}), after };
}

const results = cases.map(run);
const fixture = JSON.stringify({ generator: 'crates/server-usage/tests/record_write.cjs', sources: Object.fromEntries(READ_FILES.map((f) => [f, sha(f)])), cases: results }, null, 1) + '\n';
const check = process.argv.indexOf('--check');
if (check !== -1) {
  if (fs.readFileSync(process.argv[check + 1], 'utf8') !== fixture) { process.stderr.write('usage write fixture differs from a fresh recording\n'); process.exit(1); }
  process.stderr.write(`usage write fixture matches (${results.length} cases)\n`);
} else process.stdout.write(fixture);
