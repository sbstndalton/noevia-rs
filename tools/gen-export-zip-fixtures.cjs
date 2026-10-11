'use strict';
// Test-only oracle: invoke the actual pinned core module, never a copied ZIP implementation.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');
const core = process.argv[2];
if (!core) throw new Error('Usage: node tools/gen-export-zip-fixtures.cjs <core-checkout>');
const source = path.resolve(core, 'server/chat-export.cjs');
const revision = execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
if (revision !== '965598462d64dc04e1df55c3bb7a25d4343380e1') throw new Error('Unexpected oracle revision');
const sourceSha256 = crypto.createHash('sha256').update(fs.readFileSync(source)).digest('hex');
if (sourceSha256 !== '5cfbfd9353b3963a903f0da70fb191e50c7af9d6f215742c222143e402d62d7c') throw new Error('Oracle source differs from landed core');
const { zipStore } = require(source);
const sets = [[], [{ name: 'README.md', data: Buffer.from('synthetic\n') }], [
  { name: 'projects/Å-👨/é.md', data: Buffer.from([0, 1, 255, 13, 10]) },
  { name: 'same.md', data: Buffer.from('first') }, { name: 'same.md', data: Buffer.from('second') },
]];
const times = [Date.parse('1979-12-31T23:59:59Z'), Date.parse('1980-01-01T00:00:00Z'),
  Date.parse('2000-02-29T12:34:57Z'), 1800000000000, Date.parse('2107-12-31T23:59:58Z')];
const cases = times.flatMap(when => sets.map(entries => ({ when,
  entries: entries.map(({ name, data }) => ({ name, data: data.toString('base64') })),
  zip: zipStore(entries, when).toString('base64'),
})));
process.stdout.write(JSON.stringify({ core: revision, source: 'server/chat-export.cjs zipStore',
  sourceSha256, cases }, null, 2) + '\n');
