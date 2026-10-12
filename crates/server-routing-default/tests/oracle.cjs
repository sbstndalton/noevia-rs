#!/usr/bin/env node
'use strict';
// Test-oracle glue only (sbstndalton/noevia#1305): the actual core workspace store
// (server/workspace.cjs createWorkspaceStore) loads synthetic per-user preferences.json bytes, and
// the answer is the expression core server/routes/models.cjs GET /api/routing-default returns.
// Usage: node oracle.cjs <noevia-core checkout> <output.json>
const fs = require('node:fs'), os = require('node:os'), path = require('node:path');
const cp = require('node:child_process'), crypto = require('node:crypto');
const core = path.resolve(process.argv[2] || ''), out = process.argv[3];
if (!out) throw Error('usage: oracle.cjs <noevia-core checkout> <output.json>');
const sha = cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
if (cp.execFileSync('git', ['-C', core, 'status', '--porcelain', '--untracked-files=no'], { encoding: 'utf8' }).trim())
  throw Error('oracle checkout must be clean');
const { createWorkspaceStore } = require(path.join(core, 'server/workspace.cjs'));
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-routing-default-oracle-'));
const A = 'aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa';
const hex = (s) => Buffer.from(s, 'latin1').toString('hex');
// Synthetic bytes only. `text` is UTF-8; `hex` is raw bytes; `null` means no preferences.json.
const cases = [
  ['object-manual', { text: '{"defaultRouting":"manual"}' }],
  ['object-auto', { text: '{"defaultRouting":"auto"}' }],
  ['case-differs', { text: '{"defaultRouting":"Manual"}' }],
  ['trailing-space-value', { text: '{"defaultRouting":"manual "}' }],
  ['escaped-value', { text: '{"defaultRouting":"\\u006danual"}' }],
  ['escaped-key', { text: '{"\\u0064efaultRouting":"manual"}' }],
  ['duplicate-last-auto', { text: '{"defaultRouting":"manual","defaultRouting":"auto"}' }],
  ['duplicate-last-manual', { text: '{"defaultRouting":"auto","defaultRouting":"manual"}' }],
  ['duplicate-both-manual', { text: '{"defaultRouting":"manual","defaultRouting":"manual"}' }],
  ['null-value', { text: '{"defaultRouting":null}' }],
  ['number-value', { text: '{"defaultRouting":1}' }],
  ['array-value', { text: '{"defaultRouting":["manual"]}' }],
  ['nested-only', { text: '{"a":{"defaultRouting":"manual"}}' }],
  ['top-null', { text: 'null' }],
  ['top-array', { text: '["manual"]' }],
  ['top-string', { text: '"manual"' }],
  ['top-zero', { text: '0' }],
  ['top-false', { text: 'false' }],
  ['top-empty-string', { text: '""' }],
  ['empty-file', { text: '' }],
  ['malformed-brace', { text: '{' }],
  ['trailing-comma', { text: '{"defaultRouting":"manual",}' }],
  ['trailing-garbage', { text: '{"defaultRouting":"manual"} x' }],
  ['utf8-bom', { hex: hex('﻿{"defaultRouting":"manual"}') }],
  ['whitespace-around', { text: '\n\t {"defaultRouting" : "manual" }\n' }],
  ['huge-number-sibling', { text: '{"defaultRouting":"manual","x":1e999}' }],
  ['invalid-utf8-sibling', { hex: hex('{"note":"\xff","defaultRouting":"manual"}') }],
  ['lone-surrogate-sibling', { text: '{"x":"\\uD800","defaultRouting":"manual"}' }],
  ['missing-file', null],
];
const results = [];
let index = 0;
for (const [name, file] of cases) {
  const dir = path.join(root, String(index++));
  const store = createWorkspaceStore(dir, { id: 'default', label: 'Default', baseUrl: 'http://127.0.0.1:1', apiKey: '', shared: true }, null);
  if (file) {
    fs.mkdirSync(path.join(dir, 'users', A), { recursive: true });
    fs.writeFileSync(path.join(dir, 'users', A, 'preferences.json'), file.hex ? Buffer.from(file.hex, 'hex') : file.text);
  }
  const ws = store.get(A);
  // models.cjs: ws.preferences?.defaultRouting === 'manual' ? 'manual' : 'auto'
  const routing = ws.preferences?.defaultRouting === 'manual' ? 'manual' : 'auto';
  results.push({ name, file, expected: { routing } });
}
// Tenant isolation: a manual file for one tenant never answers for another.
{
  const dir = path.join(root, String(index++));
  const B = 'bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb';
  const file = { text: '{"defaultRouting":"manual"}' };
  fs.mkdirSync(path.join(dir, 'users', A), { recursive: true });
  fs.writeFileSync(path.join(dir, 'users', A, 'preferences.json'), file.text);
  const store = createWorkspaceStore(dir, { id: 'default', label: 'Default', baseUrl: 'http://127.0.0.1:1', apiKey: '', shared: true }, null);
  const other = store.get(B).preferences?.defaultRouting === 'manual' ? 'manual' : 'auto';
  results.push({ name: 'other-tenant-auto', tenant: A, query: B, file, expected: { routing: other } });
}
fs.writeFileSync(out, JSON.stringify({
  core: sha,
  workspaceSha256: crypto.createHash('sha256').update(fs.readFileSync(path.join(core, 'server/workspace.cjs'))).digest('hex'),
  cases: results.map((c) => ({ ...c, tenant: c.tenant || A, query: c.query || c.tenant || A })),
}, null, 1) + '\n');
fs.rmSync(root, { recursive: true, force: true });
process.stdout.write(`routing-default fixtures=${results.length} core=${sha}\n`);
