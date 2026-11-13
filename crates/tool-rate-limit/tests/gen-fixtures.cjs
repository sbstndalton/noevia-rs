'use strict';
// Records core's server/tool-rate-limit.cjs (#1693) for the Rust port (crates/tool-rate-limit). Synthetic
// inputs only, deterministic (a seeded generator and a virtual clock); nothing is contacted.
//
//   node crates/tool-rate-limit/tests/gen-fixtures.cjs <core-checkout> <out.json>
//
// The fixture carries the sha256 of the source file it was recorded from and the core commit given in
// NOEVIA_CORE_REF (if set).
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

const [core, out] = process.argv.slice(2);
if (!core || !out) throw new Error('Usage: node gen-fixtures.cjs <core-checkout> <out.json>');
const file = path.join(path.resolve(core), 'server', 'tool-rate-limit.cjs');
const rl = require(file);

let seed = 0x5eed1693;
const rand = () => { seed = (seed * 1103515245 + 12345) >>> 0; return seed / 2 ** 32; };
const pick = (xs) => xs[Math.floor(rand() * xs.length)];

// ── isRateLimitedText ──
const TEXTS = [
  '', 'HTTP 429', 'http:429', 'HTTP: 429', 'HTTP429', 'xHTTP 429', 'HTTP 4290', 'HTTP 429x', 'HTTP 429.',
  'status 429', 'status code 429', 'Status Code: 429', 'status   code  :  429', 'statuscode 429', 'status code429',
  'status code 429', 'HTTP 429', 'HTTP﻿429', 'HTTP\u0085429', 'status:429', 'status: : 429',
  'rate limit', 'rate-limited', 'Rate Limiting', 'ratelimited', 'rate_limit', 'rate  limit', 'rate limits',
  'rate limitedx', 'rate\tlimit', 'rate　limit', 'xrate limit', '_rate limit', 'rate-limiting!', 'RATE-LIMIT.',
  'too many requests', 'TOO MANY REQUESTS!', 'too many  requests', 'too many requestsx', 'Too Many Requests (429)',
  'quota exceeded', 'quota exhausted', 'Quota Exceeded.', 'quota  exceeded', 'quota exceededx', 'usage limit',
  'usage limits', 'Usage Limit!', 'usage-limit', 'ſtatus 429', 'status 429é', 'érate limit',
  '😀rate limit😀', 'error 429', '429', 'Error: You exceeded your current quota exceeded',
  'x'.repeat(1995) + 'HTTP 429', 'x'.repeat(1990) + ' HTTP 429', '😀'.repeat(996) + ' rate limit', '😀'.repeat(999) + 'rate limit',
];
const TOKENS = ['HTTP', 'http', 'status', 'Status', 'code', 'CODE', '429', '4290', '1429', 'rate', 'limit', 'limited', 'limiting',
  'too', 'many', 'requests', 'quota', 'exceeded', 'exhausted', 'usage', ' ', '  ', ':', '-', '_', ' ', '\n', '.', 'x', 'é', '😀', ' '];
for (let i = 0; i < 600; i++) {
  let s = '';
  const n = 1 + Math.floor(rand() * 8);
  for (let k = 0; k < n; k++) s += pick(TOKENS);
  TEXTS.push(s);
}
const texts = TEXTS.map((text) => ({ text, limited: rl.isRateLimitedText(text) }));

// ── parseRetryAfter ──
const NOW = Date.UTC(2026, 10, 13, 4, 30, 0);
const RETRY = [null, '', ' ', '0', '5', '1.5', ' 7 ', '007', '0.0005', '2.4995', '1.', '.5', '-1', '+1', '1e3', 'abc', '5s',
  ' 5 ', '﻿12', '99999999999999999999', '1.25.3',
  'Fri, 13 Nov 2026 04:30:10 GMT', 'Fri, 13 Nov 2026 04:29:00 GMT', 'Thu, 01 Jan 1970 00:00:00 GMT', 'Sat, 29 Feb 2028 00:00:00 GMT',
  ' Fri, 13 Nov 2026 04:31:00 GMT ', '2026-11-13T04:30:30Z', '2026-11-13T04:30:30.250Z', '2026-11-13T04:00:00Z',
  'Sat, 13 Nov 2027 04:31:60 GMT', 'Wed, 31 Feb 2027 00:00:00 GMT', 'Fri, 13 nov 2026 04:31:00 gmt', 'Mon, 13 Nov 2026 04:31:00 GMT',
  'Fri, 13 Nov 2026 24:00:00 GMT', 'Fri, 13 Nov 2026 23:60:00 GMT', 'Fri, 32 Nov 2026 00:00:00 GMT', 'Fri, 00 Nov 2026 00:00:00 GMT',
  'Fri, 13 Nov 2026 04:31:00 UTC', 'Fri, 13 Nov 2026 24:00:01 GMT', '2027-02-31T00:00:00Z', '2026-11-13T04:31:60Z', '2026-11-13T24:00:00Z',
  '2026-11-13T24:00:01Z', '2026-13-01T00:00:00Z', 'Fri, 13 Nov 2026 04:31:75 GMT', 'Fri, 13 Nov 2026 24:00:60 GMT', 'Fri, 13 Xyz 2026 00:00:00 GMT',
];
const retryAfter = RETRY.map((value) => ({ value, ms: rl.parseRetryAfter(value, NOW) }));

// ── the limiter over a virtual clock ──
const sequences = [];
for (let n = 0; n < 40; n++) {
  let clock = 1_000_000;
  const limit = rl.createToolRateLimit({ now: () => clock });
  const steps = [];
  for (let k = 0; k < 60; k++) {
    const key = pick(['a', 'b', 'c', 'srv\u0000user-1']);
    const op = pick(['remaining', 'hit', 'hit', 'clear', 'advance', 'advance']);
    if (op === 'advance') {
      const by = pick([0, 1, 999, 1000, 14_999, 15_000, 15_001, 60_000, 299_999, 300_000, 300_001, 600_000, 900_000]);
      clock += by;
      steps.push({ op, by });
    } else if (op === 'remaining') {
      steps.push({ op, key, out: limit.remaining(key), size: limit.size() });
    } else if (op === 'hit') {
      const retryAfterMs = pick([null, null, 0, 1, 500, 999, 1000, 2500, 299_999, 400_000, -5]);
      steps.push({ op, key, retryAfterMs, out: limit.hit(key, retryAfterMs), size: limit.size() });
    } else {
      limit.clear(key);
      steps.push({ op, key, size: limit.size() });
    }
  }
  sequences.push(steps);
}

// ── the model's texts ──
const messages = [0, 1, 999, 1000, 1001, 14_999.5, 15_000, 299_999.5, 300_000, -5, NaN, Infinity, 1e30, 1e24].map((ms) => ({
  name: 'tavily_search', ms: Number.isFinite(ms) ? ms : String(ms), limited: rl.rateLimitedText('tavily_search', ms), cooling: rl.coolingDownText('tavily_search', ms),
}));

const source = fs.readFileSync(file);
const fixture = {
  version: 1,
  generator: 'noevia-rs crates/tool-rate-limit/tests/gen-fixtures.cjs (from noevia-core server/tool-rate-limit.cjs)',
  source: { path: 'server/tool-rate-limit.cjs', sha256: crypto.createHash('sha256').update(source).digest('hex'), coreRef: process.env.NOEVIA_CORE_REF || null },
  constants: { baseMs: rl.BASE_MS, maxMs: rl.MAX_MS },
  now: NOW,
  texts, retryAfter, sequences, messages,
};
fs.writeFileSync(out, JSON.stringify(fixture, null, 1) + '\n');
