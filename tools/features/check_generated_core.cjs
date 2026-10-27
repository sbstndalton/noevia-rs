'use strict';
// Compare the generated-core production facade to the committed actual-core oracle.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const core = process.argv[2];
if (!core) throw new Error('usage: node check_generated_core.cjs <candidate core checkout>');
const fixture = JSON.parse(fs.readFileSync(path.resolve(__dirname, '../../crates/server-features/tests/fixtures/core-feature-eval-v2.json')));
const { REGISTRY, createFeatures } = require(path.resolve(core, 'server/features.cjs'));
assert.deepEqual(Object.keys(REGISTRY), fixture.names);
const dynamic = spec => typeof spec.unavailable === 'function';
const one = (name, available) => {
  const spec = REGISTRY[name];
  return { [name]: dynamic(spec) && name !== 'nativeClientAuth'
    ? { ...spec, unavailable: () => available ? null : 'synthetic unavailable' }
    : spec };
};
const describe = (features, name) => {
  const row = features.describe()[0];
  return {
    value: row.enabled, source: row.source, locked: row.locked,
    available: row.unavailable == null, enabled: features.enabled(name),
    flag: features.flags()[name], pendingRestart: row.pendingRestart,
  };
};
for (const row of fixture.definitions) {
  const spec = REGISTRY[row.name];
  assert.deepEqual({
    name: row.name, env: spec.env, setting: 'feature:' + row.name,
    default: spec.default === true, experimental: spec.experimental === true,
    restart: spec.restart === true, legacy: spec.legacy?.name ?? null,
    label: spec.label, description: spec.description,
    dynamicAvailability: dynamic(spec),
  }, row);
}
for (const c of fixture.cases) {
  const env = { TRUST_PROXY: c.available ? 'true' : 'false' };
  if (c.envRaw !== null) env[REGISTRY[c.name].env] = c.envRaw;
  const store = { get: key => key === 'feature:' + c.name ? c.stored : null };
  let actual;
  try {
    actual = describe(createFeatures({ registry: one(c.name, c.available), env, store, log: () => {} }), c.name);
  } catch (error) {
    actual = { error: error.message };
  }
  assert.deepEqual(actual, c.expected, JSON.stringify(c));
}
for (const c of fixture.transitions) {
  const values = new Map([['feature:' + c.name, String(c.boot)]]);
  const store = { get: key => values.get(key), set: (key, value) => values.set(key, value) };
  const features = createFeatures({ registry: one(c.name, true), env: { TRUST_PROXY: 'true' }, store, log: () => {} });
  features.set(c.name, c.next, 'synthetic-admin');
  assert.deepEqual(describe(features, c.name), c.expected, JSON.stringify(c));
}
const values = new Map([['feature:astraReview', 'true']]);
const store = { get: key => values.get(key), set: (key, value) => values.set(key, value) };
const features = createFeatures({ registry: one('plannerReview', true), env: {}, store, log: () => {} });
assert.deepEqual({
  resolved: features.resolve('astraReview'),
  migrated: values.get('feature:plannerReview'),
  original: values.get('feature:astraReview'),
  before: describe(features, 'plannerReview'),
}, { resolved: fixture.legacy.resolved, migrated: fixture.legacy.migrated,
  original: fixture.legacy.original, before: fixture.legacy.before });
features.set('astraReview', false, 'synthetic-admin');
assert.deepEqual(describe(features, 'plannerReview'), fixture.legacy.after);
assert.equal(values.get('feature:plannerReview'), fixture.legacy.updated);
