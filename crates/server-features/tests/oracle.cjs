'use strict';
// Regenerate only from the selected, verified noevia-core source head.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');
const core = process.argv[2];
if (!core) throw new Error('usage: node oracle.cjs <selected noevia-core checkout>');
const sha = execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
if (sha !== '21c65f2039632a77a3827533429ff87c4a936f7b') {
  throw new Error('the selected core head changed; review a new oracle before regenerating');
}
const source = path.join(core, 'server/features.cjs');
const { REGISTRY, createFeatures } = require(source);
const names = ['nativeClientAuth', 'routingModes', 'chatgptOAuth'];
const envValues = [null, '', ' ', '1', 'true', ' ON ', 'yes', '0', 'false', 'OFF', 'no', 'invalid', '\u00a0YES\u00a0', '\u200b1'];
const storedValues = [null, 'true', 'false', 'True', '1', 1, true];
const cases = [];
for (const name of names) {
  const registry = { [name]: REGISTRY[name] };
  for (const envRaw of envValues) {
    for (const stored of storedValues) {
      for (const trustProxy of [false, true]) {
        const env = { TRUST_PROXY: trustProxy ? 'true' : 'false' };
        if (envRaw !== null) env[REGISTRY[name].env] = envRaw;
        const store = { get: key => key === 'feature:' + name ? stored : null };
        let expected;
        try {
          const features = createFeatures({ registry, env, store, log: () => {} });
          const row = features.describe()[0];
          expected = {
            name: row.name,
            env: row.env,
            value: row.enabled,
            source: row.source,
            locked: row.locked,
            available: row.unavailable == null,
            enabled: features.enabled(name),
            flag: features.flags()[name],
            pendingRestart: row.pendingRestart,
          };
        } catch (error) {
          expected = { error: error.message };
        }
        cases.push({ name, envRaw, stored, trustProxy, expected });
      }
    }
  }
}
process.stdout.write(JSON.stringify({
  source: { core: sha, featuresSha256: crypto.createHash('sha256').update(fs.readFileSync(source)).digest('hex') },
  cases,
}) + '\n');
