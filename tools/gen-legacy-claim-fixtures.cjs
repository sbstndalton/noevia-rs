'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const ORACLE = '62abda5fbaeaa62d309a3fc8e19f1096afb5591b';
const core = path.resolve(process.argv[2] || '');
const output = path.resolve(process.argv[3] || '');
if (!process.argv[2] || !process.argv[3]) throw new Error('usage: node gen-legacy-claim-fixtures.cjs CORE_DIR OUTPUT');
const revision = ORACLE;
const expectedSourceHashes = {
  'server/workspace.cjs': 'a8e4c57fb8c04f34722a4b012d18bddf3956870b623cf3151f796ac9dbb70095',
  'server/source-text.cjs': 'a5ff4a4b97be8e8173fc41279e03bb3ccae0bf234fb7458d489860737879e3cb',
};
const sourcePaths = Object.keys(expectedSourceHashes);
const sourceHashes = Object.fromEntries(sourcePaths.map((p) => [
  p,
  crypto.createHash('sha256').update(fs.readFileSync(path.join(core, p))).digest('hex'),
]));
if (JSON.stringify(sourceHashes) !== JSON.stringify(expectedSourceHashes)) {
  throw new Error('legacy claim oracle source bytes do not match the pinned 62abda5 fixture source');
}
const createWorkspaceStore = require(path.join(core, 'server/workspace.cjs')).createWorkspaceStore;
const user = '11111111-1111-4111-8111-111111111111';
const owner1 = '00000000-0000-4000-8000-000000000000';
const owner2 = '22222222-2222-4222-8222-222222222222';
const payload = {
  'projects.json': Buffer.from('{"projects":[{"id":"p","name":"legacy"}]}'),
  'providers.json': Buffer.from('{"providers":[{"id":"private","apiKey":"synthetic-secret"}]}'),
  'free-chats.json': Buffer.from('[{"id":"f","title":"free"}]'),
  'auto-roles.json': Buffer.from('{"default":"chat"}'),
  'history-room.json': Buffer.from('{"messages":[{"role":"user","text":"synthetic"}]}'),
  'rag/nested/index.bin': Buffer.from([0, 1, 2, 255]),
};
function specification(name) {
  const full = Object.entries(payload).map(([p, bytes]) => ({ path: p, hex: bytes.toString('hex') }));
  if (name === 'no-trigger') return {
    files: full.filter((f) => ['free-chats.json', 'auto-roles.json', 'rag/nested/index.bin'].includes(f.path)),
    owner: null, prior: [],
  };
  const prior = name === 'previous-owner' ? [owner1, owner2]
    : name === 'previous-owner-reverse' ? [owner2, owner1] : [];
  const owner = name === 'target-exists' ? { userId: user }
    : name === 'malformed-owner' ? '{' : null;
  const files = name === 'target-exists'
    ? full.concat([{ path: `users/${user}/keep.txt`, hex: Buffer.from('target-preserved').toString('hex') }])
    : full;
  return { files, owner, prior };
}
function writeInputs(root, spec) {
  for (const item of spec.files) {
    const file = path.join(root, item.path);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, Buffer.from(item.hex, 'hex'));
  }
  for (const id of spec.prior) {
    const marker = path.join(root, 'users', id, 'migration.json');
    fs.mkdirSync(path.dirname(marker), { recursive: true });
    fs.writeFileSync(marker, '{"version":1}');
  }
  if (spec.owner !== null) {
    fs.writeFileSync(path.join(root, 'legacy-owner.json'),
      typeof spec.owner === 'string' ? spec.owner : JSON.stringify(spec.owner));
  }
}
function canonicalPath(rel) {
  return rel.split(path.sep).join('/').replace(/migration-backups\/legacy-[^/]+/g, 'migration-backups/legacy');
}
function snapshot(root, result) {
  const tree = [];
  function visit(rel) {
    const file = path.join(root, rel);
    const stat = fs.lstatSync(file);
    const p = canonicalPath(rel);
    if (stat.isSymbolicLink()) {
      tree.push({ path: p, link: fs.readlinkSync(file) });
    } else if (stat.isDirectory()) {
      tree.push({ path: p, type: 'dir' });
      for (const name of fs.readdirSync(file).sort()) visit(path.join(rel, name));
    } else {
      const rec = { path: p, type: 'file' };
      const bytes = fs.readFileSync(file);
      if (path.basename(rel) === 'legacy-owner.json') {
        try { rec.json = JSON.parse(bytes.toString('utf8')); } catch { rec.hex = bytes.toString('hex'); }
      } else if (path.basename(rel) === 'migration.json' && rel.split(path.sep).includes('users')) {
        try {
          const marker = JSON.parse(bytes.toString('utf8'));
          if (marker.backup) marker.backup = canonicalPath(path.relative(root, marker.backup));
          delete marker.migratedAt;
          rec.json = marker;
        } catch { rec.hex = bytes.toString('hex'); }
      } else {
        rec.hex = bytes.toString('hex');
      }
      tree.push(rec);
    }
  }
  for (const name of fs.readdirSync(root).sort()) visit(name);
  return { result, tree };
}
const cases = ['ordinary', 'no-trigger', 'previous-owner', 'previous-owner-reverse', 'target-exists', 'malformed-owner'];
const results = [];
for (const name of cases) {
  const spec = specification(name);
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'legacy-claim-oracle-'));
  try {
    writeInputs(root, spec);
    const store = createWorkspaceStore(root, { id: 'default', label: 'Default', apiKey: '' }, null, { projectStore: {} });
    const result = store.claimLegacy(user);
    results.push({ name, user, files: spec.files, owner: spec.owner, prior: spec.prior, expected: snapshot(root, result) });
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}
fs.mkdirSync(path.dirname(output), { recursive: true });
fs.writeFileSync(output, JSON.stringify({
  version: 1,
  oracle: { core: revision, sources: sourceHashes },
  cases: results,
}, null, 2) + '\n');
