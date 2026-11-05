#!/usr/bin/env node
'use strict';
// Oracle for js-json's number_to_string: String(x) (ECMAScript Number::toString, as V8 writes it) for
// binary64 values where the shortest round-trip digits are ambiguous (two neighbouring digit strings
// read back as the same double, so the formatter's tie choice shows), plus boundaries and a seeded
// random sample. Values are given by their bits, so nothing is lost on the way to Rust.
//   node tools/gen-js-number-fixtures.cjs > crates/js-json/tests/fixtures/number-to-string-node.json
const view = new DataView(new ArrayBuffer(8));
const bitsOf = x => { view.setFloat64(0, x); return view.getBigUint64(0).toString(16).padStart(16, '0'); };
const fromBits = b => { view.setBigUint64(0, BigInt.asUintN(64, b)); return view.getFloat64(0); };

// Two digit strings of the same length name x: the formatter had to choose.
function ambiguous(x) {
  const s = x.toExponential();
  const [mant, exp] = s.split('e');
  const digits = mant.replace('.', '').replace('-', '');
  if (digits.length < 2) return false;
  const m = BigInt(digits);
  for (const alt of [m - 1n, m + 1n]) {
    const d = alt.toString();
    if (d.length !== digits.length) continue;
    const text = `${d[0]}.${d.slice(1)}e${exp}`;
    if (Number(text) === Math.abs(x)) return true;
  }
  return false;
}

const values = new Map();
const add = x => { if (typeof x === 'number') values.set(bitsOf(x), x); };
const ties = new Map();
for (let e = -1074; e <= 1023; e++) {
  for (const k of [1, 3, 5, 7, 9, 11, 13, 15, 17, 19, 21, 23, 25]) {
    const x = k * 2 ** e;
    if (!Number.isFinite(x) || x === 0) continue;
    for (const y of [x, fromBits(BigInt('0x' + bitsOf(x)) + 1n), fromBits(BigInt('0x' + bitsOf(x)) - 1n)]) {
      if (Number.isFinite(y) && y !== 0 && ambiguous(y)) ties.set(bitsOf(y), y);
    }
  }
}
// The review's example (2^-25), its neighbours and negatives of a sample.
for (const x of [2 ** -25, 2.98023223876953125e-8]) {
  add(x); add(-x);
  add(fromBits(BigInt('0x' + bitsOf(x)) + 1n));
  add(fromBits(BigInt('0x' + bitsOf(x)) - 1n));
}
const tieList = [...ties.values()];
// Every power of two that is ambiguous (the review's own kind), and every 25th other one, so the file
// stays small; some negated.
tieList.forEach((x, i) => { if (i % 25 === 0 || Number.isInteger(Math.log2(x))) { add(x); if (i % 7 === 0) add(-x); } });
for (const x of [0, -0, NaN, Infinity, -Infinity, 1, -1, 0.1, 0.2, 0.30000000000000004, 1e21, 1e-7, 123e-20,
  9007199254740991, 9007199254740993, 2 ** 53, 2 ** 64, 1e20, 999999999999999900000, 1.7976931348623157e308,
  5e-324, 2.2250738585072014e-308, 2.225073858507201e-308, Number.EPSILON, 4.35, 0.000001, 1.5e-7, 100, 1e300]) add(x);
// Seeded random bit patterns (xorshift64*), finite only.
let seed = 0x9e3779b97f4a7c15n;
for (let n = 0; n < 1000; n++) {
  seed ^= seed >> 12n; seed ^= BigInt.asUintN(64, seed << 25n); seed ^= seed >> 27n;
  const x = fromBits(BigInt.asUintN(64, seed * 0x2545f4914f6cdd1dn));
  if (Number.isFinite(x)) add(x);
}
const out = [...values.entries()].sort().map(([b, x]) => [b, String(x)]);
process.stderr.write(`ambiguous candidates found: ${tieList.length}; fixture values: ${out.length}; node ${process.version}\n`);
process.stdout.write(JSON.stringify({ generator: 'tools/gen-js-number-fixtures.cjs', node: process.version, values: out }) + '\n');
