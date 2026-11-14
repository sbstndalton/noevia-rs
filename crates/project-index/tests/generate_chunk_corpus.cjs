#!/usr/bin/env node
'use strict';
// Records core's chunkText (server/rag.cjs) over a generated corpus, for tests/one_writer.rs.
// The texts come from the same deterministic generator as the Rust test (a 64-bit LCG, ported with
// BigInt), so the fixture keeps only hashes: the text's sha256 (UTF-8) and the chunks' sha256 (each
// chunk's UTF-16LE code units followed by two zero bytes). Synthetic text only.
//
// Usage (on the builder): NOEVIA_CORE_CHECKOUT=<core checkout> node crates/project-index/tests/generate_chunk_corpus.cjs
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');

const core = process.env.NOEVIA_CORE_CHECKOUT;
if (!core) throw new Error('set NOEVIA_CORE_CHECKOUT');
const srv = path.join(core, 'server');
const rag = require(path.join(srv, 'rag.cjs'));

const SEED = 0x5eed2677n;
const CASES = 600;
const MASK = (1n << 64n) - 1n;
class Gen {
  constructor(seed) { this.s = seed; }
  next() {
    this.s = (this.s * 6364136223846793005n + 1442695040888963407n) & MASK;
    return this.s >> 33n;
  }
  pick(xs) { return xs[Number(this.next() % BigInt(xs.length))]; }
  text() {
    const PIECES = ['qa', 'notes', 'sample', 'word', 'x', 'longerwordwithoutspaces', ' ', ' ', ' ', '. ', '! ',
      '? ', '.', '\n', '\n\n', '\r\n', '\r\n\r\n', '  ', '\t', ' ', ' ', '　',
      '﻿', '\u{1f600}', 'é', '[Page 2]\n', '[Page 10]', '[Page ]\n', '[Page 7] '];
    const target = Number(this.next() % 7000n);
    let s = '';
    if (this.next() % 3n === 0n) s += this.pick(['[Page 1]\n', '[Page 1]', '[Page 12]\n\n', '  [Page 1]\n']);
    while (s.length < target) {
      s += this.pick(PIECES);
      if (this.next() % 400n === 0n) s += `\n[Page ${this.next() % 30n}]\n`;
    }
    return s;
  }
}

const sha = (buf) => crypto.createHash('sha256').update(buf).digest('hex');
function chunksHash(chunks) {
  const parts = [];
  for (const c of chunks) {
    const b = Buffer.alloc(c.length * 2 + 2);
    for (let i = 0; i < c.length; i++) b.writeUInt16LE(c.charCodeAt(i), i * 2);
    parts.push(b);
  }
  return sha(Buffer.concat(parts));
}

const g = new Gen(SEED);
const cases = [];
for (let i = 0; i < CASES; i++) {
  const text = g.text();
  const chunks = rag.chunkText(text);
  cases.push({ text: sha(Buffer.from(text, 'utf8')), n: chunks.length, chunks: chunksHash(chunks) });
}
const out = {
  note: 'core chunkText over the generated corpus of tests/one_writer.rs; written by tests/generate_chunk_corpus.cjs',
  sources: { 'server/rag.cjs': sha(fs.readFileSync(path.join(srv, 'rag.cjs'))) },
  seed: Number(SEED),
  cases,
};
fs.writeFileSync(path.join(__dirname, 'fixtures', 'chunk-corpus.json'), JSON.stringify(out, null, 1) + '\n');
console.log(`chunk-corpus.json: ${cases.length} cases, ${cases.reduce((a, c) => a + c.n, 0)} chunks`);
