#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own chat-model-kind.cjs `isChatGenerationModel` and
// model-system.cjs `isSystemModel` (with its roles file read through `useRolesBeside` +
// `refreshRoles`, the path the live server takes) on synthetic names, labels, model paths and
// roles-file bytes, and records each answer. sbstndalton/noevia#2584.
//
//   node tools/gen-model-kind-fixtures.cjs <noevia-core checkout> <output.json>
//
// bins/noevia-server/tests/model_kind_node.rs replays every case against the front's model_kind.rs.
'use strict';
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const cp = require('node:child_process');

const core = path.resolve(process.argv[2] || '../noevia-core');
const out = process.argv[3];
if (!out) throw Error('usage: gen-model-kind-fixtures.cjs <core> <output.json>');
const sha = cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
const system = require(path.join(core, 'server', 'model-system.cjs'));
const { isChatGenerationModel } = require(path.join(core, 'server', 'chat-model-kind.cjs'));
// The deprecated name rule logs once per name; the recording does not need it.
console.warn = () => {};

// Roles files as bytes (hex in the fixture). null: no file beside models.ini.
const enc = (t) => Buffer.from(t, 'utf8');
const ROLES = [
  null,
  enc('{"qa-sys":"system-routing"}'),
  enc('{"qa-sys":"system-routing\uFEFF"}'),
  enc('{"qa-sys":" SYSTEM-ROUTING\u0085"}'),
  enc('{"qa-sys":"\u3000Laya\u00A0"}'),
  enc('{"qa-router":"system-routing","x":"\\ud800"}'),
  Buffer.concat([enc('{"qa-router":"system-routing","x":"'), Buffer.from([0xff, 0xc3]), enc('"}')]),
  // Bad bytes in keys: 0xff alone, and a cut three-byte sequence (one U+FFFD each, as WHATWG decodes).
  Buffer.concat([enc('{"qa-router":"laya","qa-bad'), Buffer.from([0xff]), enc('":"chat","'), Buffer.from([0xe2, 0x82]), enc('":"system-routing"}')]),
  enc('{"\\ud800":"chat","qa-router":"laya"}'),
  enc('{"laya":"chat","qa-sys":5,"qa-chat":"chat"}'),
  enc('\uFEFF{"qa-router":"system-routing"}'),
  enc('{"qa-sys":"system-routing","qa-sys":7}'),
  enc('{"qa-sys":"chat","qa-sys":"system-routing"}'),
  enc('{"*":"system-routing","qa-nested":{"role":"system-routing"}}'),
  enc('{"qa-sys":"\u212Aelvin"}'),
  enc('[1]'),
  enc('not json'),
];
const NAMES = [
  'gemma-chat', 'qa-model', 'laya', 'Laya_v2', ' laya-1 ', 'layaway', '\uFEFFlaya', 'laya\u0085',
  '\u3000laya\u00A0', 'laya\u2028', 'qa-reran\u212A', 'qa-EMBED', 'qa-\u0130rerank', 'qa-sys',
  'qa-sys\uFEFF', 'qa-sys\u0085', ' qa-sys', 'qa-router', 'qa-chat', 'qa-bad\uFFFD', '\uFFFD', 'qa-nested', 'x', '*', '',
];
const LABELS = [
  [], ['embeddings'], ['EMBEDDING'], ['reran\u212A'], ['reranker\uFEFF'], ['vision'], ['Reranking'],
];
const PATHS = [
  '/models/Laya-1.gguf', '/models/qa.gguf', 'C:\\m\\LAYA.GGUF', '/m/\uFEFFlaya.gguf', '/m/laya\u0085.gguf',
  '/m/qa-sys/laya_x', '',
];

const root = fs.mkdtempSync(path.join(os.tmpdir(), 'model-kind-fixture-'));
const cases = [];
ROLES.forEach((bytes, i) => {
  const dir = path.join(root, `r${i}`);
  fs.mkdirSync(dir);
  if (bytes) fs.writeFileSync(path.join(dir, system.ROLES_FILENAME), bytes);
  system.useRolesBeside(path.join(dir, 'models.ini'));
  const roles = bytes ? bytes.toString('hex') : null;
  for (const name of NAMES) {
    cases.push({
      roles, name, labels: [], path: null,
      system: system.isSystemModel(name), chat: isChatGenerationModel(name, []),
    });
  }
  // The path rule only applies to a name without a role or a Laya token.
  for (const name of ['qa-model', 'qa-sys']) {
    for (const p of PATHS) {
      cases.push({ roles, name, labels: [], path: p, system: system.isSystemModel(name, p), chat: null });
    }
  }
});
system.useRolesBeside(path.join(root, 'none', 'models.ini'));
for (const name of ['qa-model', 'laya', 'qa-reran\u212A']) {
  for (const labels of LABELS) {
    cases.push({ roles: null, name, labels, path: null, system: null, chat: isChatGenerationModel(name, labels) });
  }
}
fs.rmSync(root, { recursive: true, force: true });
const node = `v${process.versions.node.split('.')[0]}`;
fs.writeFileSync(out, `${JSON.stringify({ core: sha, node, cases }, null, 1)}\n`);
console.log(`model kind fixtures=${cases.length} core=${sha} node=${process.version}`);
