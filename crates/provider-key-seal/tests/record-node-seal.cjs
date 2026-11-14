'use strict';
// Test-only Node oracle for crates/provider-key-seal: core's seal-on-load migration of a plaintext
// provider key (server/workspace.cjs isPlaintextKey, the load's legacyKeys = needsKeySeal(), and
// sealLegacyProviders), run with core's own createWorkspaceStore at the checkout given, over
// core's in-process project store stand-in (tests/server/project-store-stub.cjs: version = sha256 of
// the text, compare-and-swap writes). Synthetic data only; the sealer is a stub whose "envelope" is
// `enc:v1:stub-` plus the plaintext's UTF-8 hex (no key, no cipher).
//
//   node crates/provider-key-seal/tests/record-node-seal.cjs <core-checkout> <out.json>
//   node crates/provider-key-seal/tests/record-node-seal.cjs <core-checkout> --check <fixture.json>
//
// Per case: the file's text (null: no file), the front's writes made just before each of Node's
// writes (`races`, null: none), a failing read or write, and the sealer (`stub`, `none`: the
// workspace has no secrets, `throws`: encrypt throws for the plaintext `synthetic-throw`).
// Recorded: needsKeySeal() after the first get(), every text Node wrote, the store reads, the file
// at the end, the one `[providers] could not seal` warning kind, and the reads of a second
// migration started at once (0 inside the quiet window). tests/replay.rs replays it.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const args = process.argv.slice(2);
const core = args[0];
const check = args[1] === '--check';
const out = check ? args[2] : args[1];
if (!core || !out) throw new Error('Usage: node record-node-seal.cjs <core-checkout> (<out.json> | --check <fixture.json>)');
const coreRoot = path.resolve(core);
const { createWorkspaceStore } = require(path.join(coreRoot, 'server', 'workspace.cjs'));
const { createStubStore } = require(path.join(coreRoot, 'tests', 'server', 'project-store-stub.cjs'));

const USER = 'bbbbbbbb-2222-4222-8222-222222222222';
const DEFAULT = { id: 'default', label: 'Default', baseUrl: 'http://localhost', apiKey: '' };
const THROW = 'synthetic-throw';

function sealerOf(kind) {
  if (kind === 'none') return null;
  return {
    encrypt(value) {
      if (kind === 'throws' && value === THROW) throw new Error('synthetic sealer failure');
      return `enc:v1:stub-${Buffer.from(String(value), 'utf8').toString('hex')}`;
    },
    decrypt(value) {
      const m = /^enc:v1:stub-([0-9a-f]*)$/.exec(String(value));
      if (m) return Buffer.from(m[1], 'hex').toString('utf8');
      if (/^enc:/.test(String(value))) throw new Error('unreadable');
      return value;
    },
  };
}

async function run(c) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'provider-key-seal-oracle-'));
  try {
    const dir = path.join(root, 'users', USER);
    fs.mkdirSync(dir, { recursive: true });
    const file = path.join(dir, 'providers.json');
    if (c.file !== null) fs.writeFileSync(file, c.file);
    const inner = createStubStore(root);
    const races = [...(c.races || [])];
    let reads = 0;
    const written = [];
    const store = {
      ...inner,
      async read(user, name) {
        if (name === 'providers.json') {
          reads += 1;
          if (c.failRead) throw new Error('synthetic store read failure');
        }
        return inner.read(user, name);
      },
      async write(user, name, version, text) {
        if (name === 'providers.json') {
          if (c.failWrite) throw new Error('synthetic store write failure');
          const race = races.shift();
          if (race !== undefined && race !== null) fs.writeFileSync(file, race);
          const saved = await inner.write(user, name, version, text);
          if (saved.ok) written.push(text);
          return saved;
        }
        return inner.write(user, name, version, text);
      },
    };
    const warned = [];
    const warn = console.warn;
    console.warn = (...a) => { const line = a.join(' '); if (line.startsWith('[providers] could not seal')) warned.push(line); };
    let needsKeySeal, readsSecond;
    try {
      const ws = createWorkspaceStore(root, DEFAULT, sealerOf(c.sealer), { projectStore: store }).get(USER);
      needsKeySeal = ws.needsKeySeal();
      // get() started the migration if the load saw a plaintext key; this joins it, or starts one.
      await ws.sealLegacyProviders();
      await new Promise((resolve) => setImmediate(resolve));
      const before = reads;
      await ws.sealLegacyProviders();
      await new Promise((resolve) => setImmediate(resolve));
      readsSecond = reads - before;
    } finally { console.warn = warn; }
    const warning = warned.length === 0 ? null : warned.map((w) => (/kept changing/.test(w) ? 'conflict' : 'failed'));
    return {
      name: c.name, file: c.file, races: c.races || [], failRead: !!c.failRead, failWrite: !!c.failWrite, sealer: c.sealer,
      needsKeySeal, reads: reads - readsSecond, writes: written,
      final: fs.existsSync(file) ? fs.readFileSync(file, 'utf8') : null,
      warning, readsSecond,
    };
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
}

const rows = (list) => JSON.stringify({ providers: list }, null, 2);
const P = (id, apiKey) => ({ id, label: id, baseUrl: `https://${id}.test/v1`, apiKey, shared: false });
const PLAIN = rows([P('plain', 'synthetic-plain-key')]);
const HAND = [
  { name: 'one plaintext key', file: PLAIN },
  { name: 'plain, sealed and empty keys', file: rows([P('plain', 'synthetic-plain-key'), P('sealed', 'enc:v1:stub-6b'), P('none', '')]) },
  { name: 'nothing to seal (L5: bound, number, empty)', file: rows([P('bound', 'enc:v2:AAAA'), P('number', 12345), P('none', '')]) },
  { name: 'no file', file: null },
  { name: 'empty file', file: '' },
  { name: 'not json', file: '{"providers": [' },
  { name: 'bom', file: '﻿' + PLAIN },
  { name: 'top-level null', file: 'null' },
  { name: 'top-level array', file: '[{"apiKey":"synthetic-plain-key"}]' },
  { name: 'top-level string', file: '"synthetic-plain-key"' },
  { name: 'top-level number', file: '5' },
  { name: 'providers null', file: '{"providers":null}' },
  { name: 'providers object', file: '{"providers":{"0":{"apiKey":"synthetic-plain-key"}}}' },
  { name: 'providers string', file: '{"providers":"synthetic-plain-key"}' },
  { name: 'providers true', file: '{"providers":true}' },
  { name: 'null row beside a plaintext key', file: '{"providers":[null,{"id":"p","apiKey":"synthetic-plain-key"}]}' },
  { name: 'primitive rows beside a plaintext key', file: '{"providers":[5,"row",false,[],{"id":"p","apiKey":"synthetic-plain-key"}]}' },
  { name: 'array row only', file: '{"providers":[[],{"id":"p","apiKey":""}]}' },
  { name: 'duplicate apiKey, last plaintext', file: '{"providers":[{"id":"p","apiKey":"enc:v1:x","apiKey":"synthetic-plain-key"}]}' },
  { name: 'duplicate apiKey, last sealed', file: '{"providers":[{"id":"p","apiKey":"synthetic-plain-key","apiKey":"enc:v1:x"}]}' },
  { name: 'envelope prefixes', file: rows([P('a', 'enc:'), P('b', 'enc:v'), P('c', 'enc:v:x'), P('d', 'enc:v1'), P('e', 'ENC:v1:x'), P('f', ' enc:v1:x'), P('g', 'enc:v10:x'), P('h', 'enc:v١:x')]) },
  { name: 'numbers and key order rewritten', file: '{"z":1e2,"providers":[{"apiKey":"synthetic-plain-key","10":0,"2":-0,"f":1.50}],"a":0.1,"big":12345678901234567890}' },
  { name: 'lone surrogates', file: '{"providers":[{"id":"p","label":"a\\ud83d","apiKey":"k\\udc00"}]}' },
  { name: 'unicode key', file: rows([P('u', 'schlüssel-\u{1F600}-é')]) },
  { name: 'escapes kept', file: '{"providers":[{"id":"p\\n\\t\\"q\\u0001\\u007f\\u2028","apiKey":"synthetic-plain-key"}]}' },
  { name: 'front writes meanwhile (merged again)', file: PLAIN, races: [rows([P('plain', 'synthetic-plain-key'), P('added-by-front', '')])] },
  { name: 'front seals meanwhile', file: PLAIN, races: [rows([P('plain', 'enc:v1:stub-66726f6e74')])] },
  { name: 'front deletes the row meanwhile', file: PLAIN, races: [rows([])] },
  { name: 'three conflicts then a write', file: PLAIN, races: [1, 2, 3].map((n) => rows([P('plain', 'synthetic-plain-key'), P(`r${n}`, '')])) },
  { name: 'four conflicts', file: PLAIN, races: [1, 2, 3, 4].map((n) => rows([P('plain', 'synthetic-plain-key'), P(`r${n}`, '')])) },
  { name: 'store read fails', file: PLAIN, failRead: true },
  { name: 'store write fails', file: PLAIN, failWrite: true },
  { name: 'sealer throws', file: rows([P('ok', 'synthetic-plain-key'), P('bad', THROW)]), sealer: 'throws' },
  { name: 'no secrets, plaintext key', file: PLAIN, sealer: 'none' },
  { name: 'no secrets, nothing to seal', file: rows([P('none', '')]), sealer: 'none' },
];

// Seeded files: rows mixing every key kind, row kind and layout (mulberry32, fixed seed).
function mulberry32(a) { return () => { a |= 0; a = (a + 0x6D2B79F5) | 0; let t = Math.imul(a ^ (a >>> 15), 1 | a); t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t; return ((t ^ (t >>> 14)) >>> 0) / 4294967296; }; }
const rnd = mulberry32(0x5eed15);
const pick = (list) => list[Math.floor(rnd() * list.length)];
const KEYS = ['synthetic-plain-key', '', 'enc:v1:stub-6b', 'enc:v2:AAAA', 'enc:v', 'enc:v7:', 7, null, true, 'k\ud800', 'é\u{1F600}', undefined];
function seededRow() {
  const r = rnd();
  if (r < 0.06) return pick([null, 3, 'row', false, []]);
  const row = {};
  if (rnd() < 0.5) row.id = pick(['a', 'b', 'default', '7']);
  const key = pick(KEYS);
  if (key !== undefined) row.apiKey = key;
  if (rnd() < 0.3) row.capabilities = { tokenBudgetField: 'max_tokens', '1': 1.25 };
  if (rnd() < 0.2) row.shared = rnd() < 0.5;
  return row;
}
const SEEDED = [];
for (let i = 0; i < 160; i++) {
  const n = Math.floor(rnd() * 5);
  const doc = { providers: Array.from({ length: n }, seededRow) };
  if (rnd() < 0.2) doc.version = Math.floor(rnd() * 1000) / 8;
  const file = JSON.stringify(doc, null, pick([0, 2, '\t']));
  const c = { name: `seeded ${i}`, file };
  if (rnd() < 0.15) c.races = [JSON.stringify({ providers: [...doc.providers, { id: `front-${i}`, apiKey: '' }] })];
  if (rnd() < 0.05) c.failRead = true;
  SEEDED.push(c);
}

(async () => {
  const cases = [];
  for (const c of [...HAND, ...SEEDED]) cases.push(await run({ sealer: 'stub', ...c }));
  const text = `${JSON.stringify({ version: 1, generator: 'crates/provider-key-seal/tests/record-node-seal.cjs', cases }, null, 2)}\n`;
  if (check) {
    const committed = fs.readFileSync(out, 'utf8');
    if (committed !== text) {
      fs.writeFileSync(`${out}.fresh`, text);
      console.error(`record-node-seal: ${out} differs from a fresh recording (written to ${out}.fresh)`);
      process.exit(1);
    }
    console.log(`record-node-seal: ${out} matches (${cases.length} cases)`);
  } else {
    fs.writeFileSync(out, text);
    console.log(`record-node-seal: wrote ${cases.length} cases to ${out}`);
  }
})().catch((e) => { console.error(e); process.exit(1); });
