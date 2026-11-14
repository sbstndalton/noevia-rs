#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own project-edit-target.cjs `resolveEditTarget` and
// `targetDigest` (over project-file-names.cjs, uploads.cjs classify, source-readability.cjs and
// quoted-name.cjs, with the dav-parse.wasm core pins) on synthetic, QA-named projects, and records
// each answer. Track R14 step 3: crates/edit-target-lookup/tests/node_lookup.rs replays every case.
//
//   DAV_PARSE_WASM=<dav-parse.wasm whose sha256 is core's server/dav-parse.lock pin> \
//     node tools/gen-edit-target-fixtures.cjs <noevia-core checkout> <output.json>
//   ... node tools/gen-edit-target-fixtures.cjs <noevia-core checkout> --check <fixture.json>
//
// Build the module from the lock's NOEVIA_RS_REF with tools/build-dav-parse-wasm.sh. Core checks its
// sha256 against the lock when it loads it, so a wrong module cannot record. A source's text is
// inline (`content`) here, so the Rust side reads it with the same closure. Each core file loaded
// is listed with its sha256.
'use strict';
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const core = path.resolve(process.argv[2] || '');
const checkAt = process.argv[3] === '--check' ? process.argv[4] : null;
const out = checkAt ? null : process.argv[3];
if (!process.argv[2] || (!out && !checkAt)) throw Error('usage: gen-edit-target-fixtures.cjs <core> <output.json> | --check <fixture.json>');
if (!process.env.DAV_PARSE_WASM) throw Error('DAV_PARSE_WASM must name the pinned dav-parse.wasm');
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = () => {};
const edits = require(path.join(core, 'server', 'project-edit-target.cjs'));
const pretty = (v) => JSON.stringify(v, null, 2);

// ── Synthetic projects ──────────────────────────────────────────────────────
const F = 'noevia projects/QA Alpha';
const alpha = {
  id: 'qa-alpha', projectFolder: F,
  files: [
    { name: `${F}/Text/notes.md`, source: F, attachment: { state: 'ready', group: 'Text' } },
    { name: `${F}/Images/pic.md`, source: F, attachment: { state: 'ready', group: 'Images' } },
    { name: `${F}/Text/old.pdf`, source: F, attachment: { state: 'stored', group: 'Documents' } },
    { name: 'sync/a.txt', source: 'Shared/QA' },
    { name: 'dir/b.txt' },
    { name: 'plain.txt' },
    { name: 'image.png' },
    { name: 'orig.pdf', attachment: { state: 'stored', group: 'Documents' } },
    { name: 'queued.txt', document: { state: 'queued' } },
    { name: 'queued-text.txt', document: { state: 'queued' }, content: 'QA text' },
    { name: 'failed.txt', document: { state: 'failed' }, content: ' \n ' },
    { name: 'failed-text.txt', document: { state: 'failed' }, content: 'QA' },
    { name: 'doc.txt', document: { state: 'done' } },
    { name: 'doc-true.txt', document: true },
    { name: 'part.txt', attachment: { state: 'partial', group: 'Text' } },
    { name: 'ready.bin', attachment: { state: 'ready' } },
    { name: 'ready.txt', attachment: { state: 'ready', group: 'Text' } },
    { name: 'att-num.txt', attachment: 5 },
    { name: 'x/dup.md' },
    { name: 'y/dup.md' },
    { name: 'qa ‮\ud800.txt', document: { state: 'queued' } },
    { name: 'qa-lone-\udc01.md' },
    { name: 'emoji-\u{1f600}.txt', document: { state: 'failed' } },
    null, 5, 'stray', { name: 7 }, { title: 'no name' },
  ],
};
// Names that resolve (no stored name with a lone surrogate or a non-inert tail before them), the
// entries without a string name first, so a filtered index is never the full one.
const G = 'noevia projects/QA Beta';
const beta = {
  id: 'qa-beta', projectFolder: G,
  files: [
    null, 5, 'stray', { name: 7 }, { title: 'no name' },
    { name: 'x/dup.md' }, { name: 'y/dup.md' },
    { name: `${G}/Text/notes.md`, source: G, attachment: { state: 'ready', group: 'Text' } },
    { name: 'qa\u0085q.txt', document: { state: 'queued' } },
    { name: 'emoji-\u{1f600}.txt', document: { state: 'failed' } },
    { name: 'qa\u2028\u202e-dir.txt', document: { state: 'failed' } },
    { name: 't.txt' }, { name: `${G}/Text/t.txt`, source: G, attachment: { state: 'ready', group: 'Text' } },
    { name: 'free.txt' },
  ],
};
const emptyName = { id: 'qa-empty-name', files: [{ name: '' }] };
const reserved = { id: 'qa-res', reservedFolder: 'noevia projects/QA Res', files: [{ name: 'r.txt' }, { name: 'r2.txt' }, { name: 'noevia projects/QA Res/Text/r2.txt' }, { name: 'r3.md', document: { state: 'queued' } }] };
const both = { id: 'qa-both', projectFolder: 'noevia projects/QA Both', reservedFolder: 'noevia projects/QA Other', files: [{ name: 'b.txt' }] };
const noFolder = { id: 'qa-none', projectFolder: '', files: [{ name: 'n.txt' }, { name: 'n.csv' }] };
const noFiles = { id: 'qa-empty' };
const filesString = { id: 'qa-str', files: 'not a list' };
const filesEmpty = { id: 'qa-zero', files: [] };

const ACCOUNTS = [null, '', 'qa-account-digest-1'];
const cases = [];
const add = (project, args, account) => cases.push({ project, args, account });
const nameArgs = (name) => JSON.stringify({ name });
for (const f of alpha.files) if (f && typeof f.name === 'string') for (const a of ACCOUNTS) add(alpha, nameArgs(f.name), a);
for (const p of [reserved, both, noFolder]) for (const f of p.files) for (const a of ACCOUNTS) add(p, nameArgs(f.name), a);
for (const n of ['dup.md', 'notes.md', 'qa\u0085q.txt', 'emoji-\u{1f600}.txt', 'qa\u2028\u202e-dir.txt', 't.txt', 'free.txt', 'x/dup.md', 'missing.md',
  'q'.repeat(199) + '\u{1f600}', 'q'.repeat(201)]) for (const a of ACCOUNTS) add(beta, nameArgs(n), a);
for (const args of ['{"name":{"toString":1}}', '{"name":{"toString":1,"a":2}}', '{"name":[{"toString":1}]}']) add(beta, args, null);
add(emptyName, nameArgs('a.txt'), null);
for (const p of [false, 0, '']) add(p, nameArgs('a.txt'), null);
for (const p of [noFiles, filesString, filesEmpty]) add(p, nameArgs('a.txt'), 'qa-account-digest-1');
for (const args of ['', '{}', 'not json', '[]', 'null', '5', '"plain.txt"', '{"name":5}', '{"name":null}', '{"name":true}', '{"name":{"a":1}}',
  '{"name":[1,[2,null],"x"]}', '{"name":1e21}', '{"name":0.1}', '{"name":"../x"}', '{"name":"/abs.txt"}', '{"name":"%2e%2e/a"}', '{"name":"missing.txt"}',
  '{"name":" plain.txt "}', '{"name":"dup.md"}', '{"name":"notes.md"}', '{"name":"Plain.TXT"}', `{"name":"${'q'.repeat(300)}"}`, '{"name":"\\ud800x"}',
  '{"name":"qa-lone-\\udc01.md"}', '{"name":"plain.txt","extra":1}', '{"name":"plain.txt"} trailing', '{"toString":1,"name":"plain.txt"}']) {
  add(alpha, args, null);
  add(alpha, args, 'qa-account-digest-1');
}
for (const a of ACCOUNTS) add(null, nameArgs('plain.txt'), a);

function run(c) {
  const found = edits.resolveEditTarget(c.project === null ? null : JSON.parse(JSON.stringify(c.project)), c.args, { storageAccount: c.account });
  if (found.error !== undefined) return { error: String(found.error) };
  return { path: found.path, account: found.account || null, digest: edits.targetDigest(found.path, found.account || null) };
}

const DIGESTS = [['a', null], ['a', ''], ['a', 'acct'], ['noevia projects/QA/Text/\ud800.txt', 'acct-\udc00'], ['emoji \u{1f600}', null], ['', null]];

function sources() {
  const server = `${path.join(core, 'server')}${path.sep}`;
  const loaded = Object.keys(require.cache).filter((k) => k.startsWith(core + path.sep));
  if (loaded.some((k) => k.includes(`${path.sep}node_modules${path.sep}`))) throw Error('a module from node_modules was loaded');
  return Object.fromEntries(loaded.filter((k) => k.startsWith(server)).map((k) => path.relative(core, k).split(path.sep).join('/')).sort()
    .map((f) => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(core, f))).digest('hex')]));
}

const recorded = cases.map((c) => ({ project: c.project === null ? null : JSON.stringify(c.project), args: c.args, account: c.account, node: run(c) }));
const digests = DIGESTS.map(([p, a]) => ({ path: p, account: a, digest: edits.targetDigest(p, a) }));
const wasm = crypto.createHash('sha256').update(fs.readFileSync(process.env.DAV_PARSE_WASM)).digest('hex');
const doc = { version: 1, generator: 'tools/gen-edit-target-fixtures.cjs', wasmSha256: wasm, sourcesRead: sources(), cases: recorded, digests };
const text = `${pretty(doc)}\n`;
if (checkAt) {
  if (fs.readFileSync(checkAt, 'utf8') !== text) { process.stderr.write(`${checkAt} differs from a fresh recording\n`); process.exit(1); }
  process.stderr.write(`${checkAt}: identical\n`);
} else fs.writeFileSync(out, text);
