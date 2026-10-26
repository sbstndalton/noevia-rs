'use strict';
// An independent pinned-source comparison, not the generated mirror.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

const core = process.argv[2];
if (!core) throw new Error('usage: node check_core_catalog.cjs <selected core checkout>');
const catalog = JSON.parse(fs.readFileSync(path.resolve(__dirname, '../../contracts/features/catalog.json'), 'utf8'));
assert.equal(catalog.version, 1);
const sha = execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
assert.equal(sha, catalog.source.commit, 'selected core source must be the reviewed one');
const source = path.join(core, 'server/features.cjs');
assert.equal(crypto.createHash('sha256').update(fs.readFileSync(source)).digest('hex'), catalog.source.featuresSha256);
const { REGISTRY } = require(source);
assert.equal(Object.keys(REGISTRY).length, catalog.features.length);
const unavailableKinds = {
  stepSupervision: 'decisionChoice', systemOneRouting: 'systemOne',
  toolGate: 'decisionChoice', browserExecutor: 'browser',
  codePipeline: 'codeSandbox', codeMerge: 'codeSandbox',
  chatFraming: 'decisionChoice', framingReasoner: 'decisionChoice',
  nativeClientAuth: 'trustProxy',
};
for (const [i, [id, spec]] of Object.entries(REGISTRY).entries()) {
  const row = catalog.features[i];
  assert.equal(row.id, id);
  assert.equal(row.env, spec.env);
  assert.equal(row.default, spec.default === true);
  assert.equal(row.experimental, spec.experimental === true);
  assert.equal(row.restart, spec.restart === true);
  assert.equal(row.legacy, spec.legacy?.name ?? null);
  assert.equal(row.label, spec.label);
  assert.equal(row.description, spec.description);
  assert.equal(row.availability, unavailableKinds[id] ?? 'none');
  assert.equal(typeof spec.unavailable === 'function', row.availability !== 'none');
}
