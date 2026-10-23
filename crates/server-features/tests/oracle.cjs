'use strict';
// Frozen actual-core oracle for the full static feature catalog and evaluator.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

const core = process.argv[2];
if (!core) throw new Error('usage: node oracle.cjs <selected noevia-core checkout>');
const sha = execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
if (sha !== '21c65f2039632a77a3827533429ff87c4a936f7b') {
  throw new Error('selected core head changed; review a new oracle before regenerating');
}
const source = path.join(core, 'server/features.cjs');
const { REGISTRY, createFeatures } = require(source);
const names = Object.keys(REGISTRY);
if (names.length !== 23) throw new Error('selected core feature registry changed');
const envValues = [null, '', ' ', '1', 'true', ' ON ', 'yes', '0', 'false', 'OFF', 'no', 'invalid', '\u00a0YES\u00a0', '\u200b1'];
const storedValues = [null, 'true', 'false', 'True', '1', 1, true];
const dynamic = spec => typeof spec.unavailable === 'function';
const describe = (features, name) => {
  const row = features.describe()[0];
  return {
    value: row.enabled, source: row.source, locked: row.locked,
    available: row.unavailable == null, enabled: features.enabled(name),
    flag: features.flags()[name], pendingRestart: row.pendingRestart,
  };
};
const one = (name, available) => {
  const spec = REGISTRY[name];
  return { [name]: dynamic(spec) && name !== 'nativeClientAuth'
    ? { ...spec, unavailable: () => available ? null : 'synthetic unavailable' }
    : spec };
};
const definitions = names.map(name => {
  const spec = REGISTRY[name];
  return {
    name, env: spec.env, setting: 'feature:' + name, default: spec.default === true,
    experimental: spec.experimental === true, restart: spec.restart === true,
    legacy: spec.legacy?.name ?? null, label: spec.label,
    description: spec.description, dynamicAvailability: dynamic(spec),
  };
});
const cases = [];
for (const name of names) {
  for (const envRaw of envValues) {
    for (const stored of storedValues) {
      for (const available of [false, true]) {
        const env = { TRUST_PROXY: available ? 'true' : 'false' };
        if (envRaw !== null) env[REGISTRY[name].env] = envRaw;
        const store = { get: key => key === 'feature:' + name ? stored : null };
        let expected;
        try {
          const features = createFeatures({ registry: one(name, available), env, store, log: () => {} });
          expected = describe(features, name);
        } catch (error) {
          expected = { error: error.message };
        }
        cases.push({ name, envRaw, stored, available, expected });
      }
    }
  }
}
const transitions = [];
for (const name of names) {
  for (const boot of [false, true]) {
    for (const next of [false, true]) {
      const values = new Map([['feature:' + name, String(boot)]]);
      const store = { get: key => values.get(key), set: (key, value) => values.set(key, value) };
      const registry = one(name, true);
      const env = { TRUST_PROXY: 'true' };
      const features = createFeatures({ registry, env, store, log: () => {} });
      features.set(name, next, 'synthetic-admin');
      transitions.push({ name, boot, next, expected: describe(features, name) });
    }
  }
}
const migrated = new Map([['feature:astraReview', 'true']]);
const legacyStore = { get: key => migrated.get(key), set: (key, value) => migrated.set(key, value) };
const legacyFeatures = createFeatures({ registry: one('plannerReview', true), env: {}, store: legacyStore, log: () => {} });
const legacy = {
  resolved: legacyFeatures.resolve('astraReview'),
  migrated: migrated.get('feature:plannerReview'),
  original: migrated.get('feature:astraReview'),
  before: describe(legacyFeatures, 'plannerReview'),
};
legacyFeatures.set('astraReview', false, 'synthetic-admin');
legacy.after = describe(legacyFeatures, 'plannerReview');
legacy.updated = migrated.get('feature:plannerReview');
process.stdout.write(JSON.stringify({
  source: { core: sha, featuresSha256: crypto.createHash('sha256').update(fs.readFileSync(source)).digest('hex') },
  names, definitions, cases, transitions, legacy,
}) + '\n');
