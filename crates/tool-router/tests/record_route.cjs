#!/usr/bin/env node
'use strict';
// Records core's own `route` answers for tests/node_route_oracle.rs.
//
//   node crates/tool-router/tests/record_route.cjs <noevia-core checkout> > crates/tool-router/tests/fixtures/node-route.v1.json
//
// Run it on the build host (n5run) or in CI, never on MS02. The cases are generated from a fixed
// seed: random rows over a small id alphabet chosen to exercise localeCompare ties (case, '-', '_',
// digits), shared tool names, `requires` chains and cycles, autoLoad 'never', missing embeddings,
// user selections and fallbacks with duplicates and unknown ids, undefined caps, fractional and
// negative topK. An undefined maxTools/topK/threshold is recorded as an absent key.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

const core = process.argv[2];
if (!core) { process.stderr.write('usage: record_route.cjs <noevia-core checkout>\n'); process.exit(2); }
const file = path.join(core, 'server', 'tool-router.cjs');
const { route } = require(path.resolve(file));
const sha256 = crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');

let seed = 0x2f6e2b1;
const rand = () => { seed = (seed * 1103515245 + 12345) >>> 0; return seed / 0x100000000; };
const pick = (xs) => xs[Math.floor(rand() * xs.length)];
const IDS = ['a', 'A', 'b', 'b-x', 'b_x', 'bx', 'B-x', 'box2', 'box10', '2', '10', 'files', 'files-mirror', 'web.search'];
const TOOLS = ['t_read', 't_write', 't_list', 't_find', 't_x', 't_y', 't_z'];
const VECS = [[1, 0, 0], [0, 1, 0], [0, 0, 1], [0.6, 0.8, 0], [0.6, 0.8, 0], [0.9, 0.1, 0], [0, 0.95, 0.1], [-1, 0, 0], [0, 0, 0], [1, 1]];

function genCase(i) {
  const n = 1 + Math.floor(rand() * 7);
  const boxes = [];
  for (let k = 0; k < n; k++) {
    const row = { id: pick(IDS), tools: [...new Set(Array.from({ length: Math.floor(rand() * 3) }, () => pick(TOOLS)))] };
    if (rand() < 0.15) row.autoLoad = 'never';
    if (rand() < 0.3) row.requires = Array.from({ length: 1 + Math.floor(rand() * 2) }, () => pick(IDS));
    if (rand() < 0.9) row.embedding = pick(VECS);
    boxes.push(row);
  }
  const notOffered = rand() < 0.3 ? pick(IDS) : null;
  const input = {
    boxes,
    notOffered,
    userSelection: rand() < 0.4 ? Array.from({ length: 1 + Math.floor(rand() * 3) }, () => pick(IDS)) : [],
    fallback: rand() < 0.4 ? Array.from({ length: 1 + Math.floor(rand() * 3) }, () => pick(IDS)) : [],
  };
  input.taskEmbedding = rand() < 0.12 ? (rand() < 0.5 ? null : []) : pick(VECS);
  if (rand() < 0.85) input.maxTools = pick([0, 1, 2, 3, 5, 20, 2.5]);
  if (rand() < 0.6) input.topK = pick([0, 1, 2, 3, 5, -1, -2, 1.7, -0.5]);
  if (rand() < 0.6) input.threshold = pick([0, 0.3, 0.35, 0.5, 0.99, -1, 1]);
  return { name: `generated-${String(i).padStart(3, '0')}`, input };
}

const cases = [];
for (let i = 0; i < 300; i++) {
  const c = genCase(i);
  const { notOffered, ...args } = c.input;
  const out = route({ ...args, offered: (id) => id !== notOffered });
  cases.push({ ...c, output: JSON.stringify(out) });
}
process.stdout.write(JSON.stringify({ version: 1, generator: 'crates/tool-router/tests/record_route.cjs', coreFile: 'server/tool-router.cjs', coreSha256: sha256, cases }, null, 1) + '\n');
