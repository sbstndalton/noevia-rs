#!/usr/bin/env node
'use strict';
// Recorded runtime limit of a provider-row turn for crates/chat-upstream src/limit.rs: what noevia-core's own
// chat-context.cjs resolveRuntimeLimit answers with no model manager (a provider row's turn, chat.cjs passes
// `manager: null` and `hosted: provider` for any row but the default) and with it off, for each contextTokens
// value a row may hold (providers.cjs validContextTokens decides), plus the local fallback (`hosted: null`).
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-upstream/tests/record_hosted_limit.cjs > crates/chat-upstream/tests/fixtures/node-hosted-limit.v1.json
//   ... --check <fixture> records again and fails unless the result is byte-identical.
// Synthetic, QA-named inputs only. The source files' sha256 are recorded.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const core = path.resolve(checkout);
const FILES = ['server/chat-context.cjs', 'server/providers.cjs'];
const SOURCES = {};
for (const rel of FILES) SOURCES[rel] = crypto.createHash('sha256').update(fs.readFileSync(path.join(core, rel))).digest('hex');
const context = require(path.join(core, 'server/chat-context.cjs'));

// Each value as the row's JSON holds it (the row is read from providers.json, so only JSON values occur);
// `undefined` = the key absent.
const VALUES = [
  undefined, null, true, false, 0, -0, 1, 2047, 2047.5, 2048, 2048.5, 2049, 4096, 8192, 32768, 131072, 1000000,
  1999999, 2000000, 2000000.5, 2000001, 4294967296, 9007199254740991, 9007199254740993, 1e21, -2048, -4096,
  1e-7, 4096.000000001, '4096', '', ' 4096', [], [4096], {}, { value: 4096 },
];
const ROW = { id: 'qa-provider', label: 'QA Provider', baseUrl: 'https://qa-provider.example/v1', apiKey: 'qa-key' };

(async () => {
  const cases = [];
  for (const value of VALUES) {
    const row = { ...ROW };
    if (value !== undefined) row.contextTokens = value;
    for (const manager of [null, { enabled: false }]) {
      const got = await context.resolveRuntimeLimit({ manager, model: 'qa-model', dir: '/nonexistent-qa', scope: 'qa', hosted: row });
      cases.push({ row: JSON.stringify(row), manager: manager ? 'off' : 'none', limit: got.limit, limitSource: got.limitSource });
    }
  }
  const local = await context.resolveRuntimeLimit({ manager: null, model: 'qa-model', dir: '/nonexistent-qa', scope: 'qa', hosted: null });
  const out = JSON.stringify({
    version: 1,
    generator: 'crates/chat-upstream/tests/record_hosted_limit.cjs',
    sources: SOURCES,
    hostedDefault: context.HOSTED_DEFAULT_LIMIT,
    local: { limit: local.limit, limitSource: local.limitSource },
    cases,
  }, null, 1) + '\n';
  const check = process.argv.indexOf('--check');
  if (check >= 0) {
    const want = fs.readFileSync(process.argv[check + 1], 'utf8');
    if (want !== out) { process.stderr.write('the recording differs from the committed fixture\n'); process.exit(1); }
    process.stderr.write(`matches (${cases.length} cases)\n`);
    return;
  }
  process.stdout.write(out);
})().catch((e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
