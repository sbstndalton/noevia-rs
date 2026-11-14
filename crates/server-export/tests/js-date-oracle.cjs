'use strict';
// Test-only Node oracle for crates/server-export js_date: `new Date(x).toISOString()` (or the error
// it throws) for a fixed corpus of JSON values, in UTC. Hand-picked forms first, then a seeded
// generator that glues V8 date-parser tokens together, so the legacy parser is crossed with
// numbers, words, signs, parentheses and separators it would rarely see by hand.
//
//   TZ=UTC node crates/server-export/tests/js-date-oracle.cjs <out.json>
//
// CI regenerates it with the pinned Node and compares with tests/fixtures/node-dates.v1.json.
process.env.TZ = process.env.ORACLE_TZ || 'UTC'; // ORACLE_TZ: a one-off run in another zone
const fs = require('node:fs');
const [out] = process.argv.slice(2);
if (!out) throw new Error('Usage: node js-date-oracle.cjs <out.json>');

const hand = [
  253402300800000, -62198755200001, 8.64e15, -8.64e15, 8.64e15 + 1, 1.9, -1.9, 0.5, 1e300, true,
  '2026-01-01', '2026-01', '2026', '+010000-01-01', '-000001-12-31T23:59:59.999Z', '-000000-01-01', '+275760-09-13T00:00:00.000Z',
  '+275760-09-13T00:00:00.001Z', '-271821-04-20T00:00:00.000Z', '-271821-04-19T23:59:59.999Z', '2026-01-01T10:00', '2026-01-01T10:00:00',
  '2026-01-01T10:00:00.5', '2026-01-01T10:00:00.1234567891Z', '2026-01-01T24:00', '2026-01-01T24:00:01', '2026-01-01T25:00',
  '2026-01-01T10:00+0130', '2026-01-01T10:00+01:30', '2026-01-01T10:00+1', '2026-01-01T10:00Z ', '2026-01-01T10:00:00z',
  '2026-02-29', '2024-02-29', '2026-02-31', '2026-13-01', '2026-00-01', '2026-1-1', '2026-01-01 10:00', '2026/01/01',
  '1/2/2026', '12/31/99', '1/2/49', '1/2/50', '2026 1 2', 'Jan 5', 'Jan 2026', '5 Jan 2026', 'January 5, 2026', 'Janx 5 2026',
  'Thu, 01 Jan 2026 10:00:00 GMT', 'Thu Jan 01 2026 10:00:00 GMT+0100 (Central European Standard Time)', 'Jan 1 2026 10:00 PM EST',
  'Jan 1 2026 12:00 AM', 'Jan 1 2026 12:00 PM', 'Jan 1 2026 13:00 PM', 'Jan 1 2026 10:00:00.250', 'Jan 1 2026 10:00:00.', 'Jan 1 2026 10::',
  'Jan 1 2026 UTC+5', 'Jan 1 2026 10:00 UTC+0530', 'Jan 1 2026 10:00 +05:30', 'Jan 1 2026 10:00 -5', 'Jan 1 2026 10:00 +123456',
  'Jan 1 2026 10:00 PDT', 'Jan 1 2026 Z', '(comment) Jan 1 2026', 'Jan (x(y)) 1 2026', 'Jan 1 2026 (', 'Jan 1 2026)', '2026-01-01)',
  'soon', '', ' ', 'T10:00', '10:00', '2026-01-01T', 'Tue', 'Tue 2026', '2026 Tue', 'x 2026', '2026 x', '1e3', '-1', '+1', '1.5',
  '000000002026-01-01', '2026-01-01T10:00:00.000+00:00', '2026-01-01t10:00z', 'JAN 1 2026', 'jan-1-2026', '1-jan-2026',
  '2026-01-01 10:00', '2026 ', '﻿2026-01-01', 'Jan　1 2026', 'Ĳan 1 2026', '2147483648', '99999999999 1 1',
  [2026, 1, 1], ['2026-01-01'], [], [[2026], [1, 1]], [null, 2026], { a: 1 }, { toString: 1 }, { valueOf: 1 }, [{ toString: 1 }],
];

// Seeded generator (mulberry32), so the corpus is deterministic.
let seed = 0x5eed2026;
const rand = () => { seed |= 0; seed = (seed + 0x6d2b79f5) | 0; let t = Math.imul(seed ^ (seed >>> 15), 1 | seed); t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t; return ((t ^ (t >>> 14)) >>> 0) / 4294967296; };
const pick = (a) => a[Math.floor(rand() * a.length)];
const pieces = [
  '2026', '1999', '99', '49', '50', '0', '00', '1', '12', '13', '31', '32', '24', '59', '60', '000123', '+002026', '-002026', '1234567890123',
  ':', '::', '-', '+', '.', ')', '(', '(x)', ' ', ',', '/', 'T', 't', 'Z', 'z', 'UTC', 'GMT', 'UT', 'EST', 'pdt', 'AM', 'pm', 'Jan', 'February',
  'dec', 'Mon', 'Tuesday', 'xyz', 'é', ' ', '\n', '!',
];
const gen = [];
// Local times around New York's 2026 DST changes (a gap on 8 March, an overlap on 1 November).
// A run in a zone east of UTC adds that zone's own 2026 change days (noevia#2344); the UTC corpus
// (the committed fixture) and the New York run stay as they were.
const eastDays = { 'Europe/Berlin': ['2026-03-29', '2026-10-25'], 'Australia/Lord_Howe': ['2026-04-05', '2026-10-04'] }[process.env.TZ] || [];
for (const d of ['2026-03-08', '2026-11-01', '2026-06-15', ...eastDays]) for (let m = 0; m < 240; m += 15) gen.push(`${d}T${String(Math.floor(m / 60)).padStart(2, '0')}:${String(m % 60).padStart(2, '0')}`);
for (let i = 0; i < 1500; i++) {
  const n = 1 + Math.floor(rand() * 9);
  let s = '';
  for (let j = 0; j < n; j++) s += pick(pieces);
  gen.push(s);
}
// Well-formed shapes with random fields, so the composers' range checks are crossed too.
const r = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const p2 = (n) => String(n).padStart(2, '0');
for (let i = 0; i < 500; i++) {
  const y = r(-3, 3) === 0 ? r(-300000, 300000) : r(1900, 2100);
  const ys = y >= 0 && y <= 9999 && rand() < 0.7 ? String(y).padStart(4, '0') : `${y < 0 ? '-' : '+'}${String(Math.abs(y)).padStart(6, '0')}`;
  const iso = `${ys}-${p2(r(0, 13))}-${p2(r(0, 32))}${rand() < 0.6 ? `T${p2(r(0, 25))}:${p2(r(0, 60))}${rand() < 0.5 ? `:${p2(r(0, 60))}.${r(0, 9999)}` : ''}${pick(['', 'Z', '+05:30', '-0800', '+24:00'])}` : ''}`;
  gen.push(iso);
  gen.push(`${pick(['Jan', 'mar', 'Sept', 'DEC'])} ${r(0, 33)} ${r(0, 3000)} ${r(0, 13)}:${p2(r(0, 61))}${pick(['', ' AM', ' PM', ' GMT', ' UTC+3', ' -0700', ' EDT'])}`);
  gen.push(`${r(0, 13)}/${r(0, 32)}/${r(0, 120)} ${r(0, 24)}:${p2(r(0, 59))}:${p2(r(0, 59))}.${r(0, 999)}`);
}

const answer = (v) => {
  try { return { iso: new Date(v).toISOString() }; } catch (e) { return { error: e.constructor.name }; }
};
const cases = [...hand, ...gen].map((v) => ({ v, ...answer(v) }));
fs.writeFileSync(out, JSON.stringify({ v: 1, node: process.version, cases }, null, 1) + '\n');
