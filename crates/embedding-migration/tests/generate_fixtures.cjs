#!/usr/bin/env node
'use strict';
// Differential fixtures for crates/embedding-migration, recorded from noevia-core's own
// server/embedding-config.cjs (loadEmbeddingConfig, with the native owner and a state file, as
// index.cjs builds it) and server/embedding-migration.cjs (createEmbeddingMigration), wholly
// synthetic.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/embedding-migration/tests/generate_fixtures.cjs > crates/embedding-migration/tests/fixtures/node-migration.v1.json
//
// Each scenario seeds an environment (and maybe a saved file) in a fresh temporary directory, then
// runs actions through the real modules. The index store, the served-model probe, the embedder and
// the native worker are stubs answering from the scenario; what the Node code asked of them (slot
// drops, kicks) and what it wrote (the state file's bytes, its log lines) is recorded per step.
// Profile names are written $P0 / $P1 (core's profileNames() order) and substituted here, and the
// temporary directory is recorded as <DIR>. Needs flock(1) (embedding-config-lock.cjs).

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const SOURCES = {
  'server/embedding-config.cjs': '03d89968f9cf77b124168f2c741b91e1ffaf5580a0dfe4a5ad299232546a29e7',
  'server/embedding-migration.cjs': 'b97076fc242eaf337f1e16813942fe5edc342e7c6594787c00aa4af752c24bdd',
  'server/embedding-served.cjs': '9ab1c0d06c60d53beef408a887c5ca4f090451e91457ae50f89ff7c029054f3b',
  'server/embedding-config-lock.cjs': 'dddeed10606f3a782a51278151651ba7c693b2816746cb115f400a81b43622d2',
  'server/embedding-profile.cjs': 'fa349dd5909a912335bb81e81b1e670e1ed774cbca626da30853e24c3743b740',
};
for (const [rel, want] of Object.entries(SOURCES)) {
  const got = crypto.createHash('sha256').update(fs.readFileSync(path.join(checkout, rel))).digest('hex');
  if (got !== want) {
    process.stderr.write(`source hash mismatch for ${rel}: ${got}\n`);
    process.exit(1);
  }
}
const server = path.join(path.resolve(checkout), 'server');
const { loadEmbeddingConfig } = require(path.join(server, 'embedding-config.cjs'));
const { createEmbeddingMigration } = require(path.join(server, 'embedding-migration.cjs'));
const { profileNames, DEFAULT_PROFILE } = require(path.join(server, 'embedding-profile.cjs'));

const P = profileNames();
const sub = (v) => JSON.parse(JSON.stringify(v ?? null).replace(/\$P(\d)/g, (_, i) => P[Number(i)]));

const A = 'qa-embed-a', B = 'qa-embed-b', C = 'qa-embed-c';
const LOCAL = 'http://10.0.0.5:8080', OTHER = 'http://10.0.0.6:8080';
const SNAP = { running: false, lastPass: { at: 1, embedded: 3, failed: 0, refused: 0, error: null }, nextServed: true, nextHeld: null, configValid: true, configInvalid: false };
const unit = (n) => Array.from({ length: 4 }, (_, i) => [1, i * n, 0.5, -0.25]);
const cov = (chunks, vectors) => ({ chunks, vectors });
const at = (profile, model, index) => `${profile}/${model}@${index}`;
const file = (active, next = null, previous = null) => JSON.stringify({ version: 1, active, next, previous }, null, 2);
const T = (model, baseUrl = null, profile = '$P0') => ({ baseUrl, model, profile });

// Scenario: env, file (initial state file text or null), rag spec, snapshot, steps.
const SCENARIOS = [
  { name: 'seed-only', env: { EMBEDDING_MODEL: A }, steps: [{ op: 'status' }] },
  { name: 'seed-defaults', env: {}, steps: [{ op: 'status' }] },
  { name: 'seed-embed-model-alias', env: { EMBED_MODEL: ` ${A} `, EMBEDDING_BASE_URL: ` ${LOCAL}/ ` }, steps: [{ op: 'status' }] },
  { name: 'seed-next-without-profile', env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: B }, steps: [{ op: 'status' }] },
  { name: 'seed-next-same-as-active', env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: A, EMBEDDING_NEXT_PROFILE: '$P0' }, steps: [{ op: 'status' }] },
  { name: 'seed-unknown-profile-kept', env: { EMBEDDING_MODEL: A, EMBEDDING_PROFILE: 'qa-unknown' }, steps: [{ op: 'status' }] },
  { name: 'pacing', env: { EMBEDDING_MODEL: A, EMBEDDING_REINDEX_DUTY: '0.2', EMBEDDING_REINDEX_BATCH: '7.5' }, steps: [{ op: 'status' }] },
  { name: 'pacing-clamped', env: { EMBEDDING_MODEL: A, EMBEDDING_REINDEX_DUTY: '9', EMBEDDING_REINDEX_BATCH: '' }, steps: [{ op: 'status' }] },
  { name: 'pacing-junk', env: { EMBEDDING_MODEL: A, EMBEDDING_REINDEX_DUTY: 'abc', EMBEDDING_REINDEX_BATCH: '0x3' }, steps: [{ op: 'status' }] },
  { name: 'pacing-low', env: { EMBEDDING_MODEL: A, EMBEDDING_REINDEX_DUTY: '-1', EMBEDDING_REINDEX_BATCH: '2.5' }, steps: [{ op: 'status' }] },
  { name: 'stale-snapshot', env: { EMBEDDING_MODEL: A }, snapshot: null, steps: [{ op: 'status' }] },
  { name: 'saved-file', env: { EMBEDDING_MODEL: C }, file: file(T(A, LOCAL), T(B, LOCAL, '$P1'), null), rag: { indexes: ['i1', 'i2'], coverage: { [at('$P0', A, 'i1')]: cov(10, 10), [at('$P0', A, 'i2')]: cov(4, 4), [at('$P1', B, 'i1')]: cov(10, 6), [at('$P1', B, 'i2')]: cov(4, 4) } }, steps: [{ op: 'status' }] },
  { name: 'damaged-json', env: { EMBEDDING_MODEL: A }, file: '{ not json', parseError: true, steps: [{ op: 'status' }, { op: 'setNext', target: T(B) }, { op: 'setNext', target: { model: '' } }, { op: 'clearNext' }, { op: 'finish' }, { op: 'rollback' }, { op: 'switch' }] },
  { name: 'damaged-version', env: { EMBEDDING_MODEL: A }, file: JSON.stringify({ version: 2, active: T(A) }), steps: [{ op: 'status' }, { op: 'setNext', target: T(B) }] },
  { name: 'damaged-null', env: { EMBEDDING_MODEL: A }, file: 'null', steps: [{ op: 'status' }] },
  { name: 'damaged-active', env: { EMBEDDING_MODEL: A }, file: JSON.stringify({ version: 1, active: { model: 'has space' } }), steps: [{ op: 'status' }] },
  { name: 'damaged-next-string', env: { EMBEDDING_MODEL: A }, file: JSON.stringify({ version: 1, active: T(A), next: 'x' }), steps: [{ op: 'status' }] },
  { name: 'saved-falsy-next', env: { EMBEDDING_MODEL: C }, file: JSON.stringify({ version: 1, active: T(A), next: 0, previous: '' }), steps: [{ op: 'status' }] },
  { name: 'saved-unknown-profile', env: { EMBEDDING_MODEL: A }, file: JSON.stringify({ version: 1, active: T(A, null, 'qa-unknown') }), steps: [{ op: 'status' }] },
  {
    name: 'set-next-served', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1'], served: { '': [A, B] } },
    steps: [{ op: 'setNext', target: T(B) }, { op: 'status' }],
  },
  {
    name: 'set-next-refusals', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1'], served: { '': [A, B], [LOCAL]: ['/models/x.gguf'], [OTHER]: ['other-model'] } },
    steps: [
      { op: 'setNext', target: null }, { op: 'setNext', target: 'qa' }, { op: 'setNext', target: {} }, { op: 'setNext', target: { model: 'two words' } },
      { op: 'setNext', target: { model: 'x'.repeat(201) } }, { op: 'setNext', target: { model: 'x'.repeat(200), profile: '$P0' } },
      { op: 'setNext', target: { model: '\u{1F600}'.repeat(100) } }, { op: 'setNext', target: { model: '\u{1F600}'.repeat(101) } },
      { op: 'setNext', target: { model: 'a\u00a0b' } }, { op: 'setNext', target: { model: 'a\u0085b' } }, { op: 'setNext', target: { model: '\ufeffqa-embed-b\ufeff' } },
      { op: 'setNext', target: { model: B, profile: 'qa-unknown' } }, { op: 'setNext', target: { model: B, profile: '  ' } }, { op: 'setNext', target: { model: B, profile: null } },
      { op: 'setNext', target: { model: B, baseUrl: 'ftp://10.0.0.5/' } }, { op: 'setNext', target: { model: B, baseUrl: 'not a url' } },
      { op: 'setNext', target: { model: B, baseUrl: 'http://u:p@10.0.0.5/' } }, { op: 'setNext', target: { model: B, baseUrl: 'http://10.0.0.5/?q=1' } },
      { op: 'setNext', target: { model: B, baseUrl: 'http://10.0.0.5/#f' } }, { op: 'setNext', target: { model: 123, baseUrl: '' } },
      { op: 'setNext', target: { model: A } }, { op: 'setNext', target: { model: B, baseUrl: `${LOCAL}///` } }, { op: 'setNext', target: { model: B, baseUrl: OTHER } },
      { op: 'setNext', target: { model: B, baseUrl: 'http://10.0.0.9:1' } }, { op: 'setNext', target: { model: true } },
    ],
  },
  { name: 'set-next-empty-query-allowed', env: { EMBEDDING_MODEL: A }, rag: { indexes: [], served: { 'http://10.0.0.5/?#': [B] } }, steps: [{ op: 'setNext', target: { model: B, baseUrl: 'http://10.0.0.5/?#' } }] },
  { name: 'set-next-latest', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1'], served: { '': ['QA-Embed-B:latest'] } }, steps: [{ op: 'setNext', target: T(B) }] },
  { name: 'set-next-served-throws', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1'], served: { '': 'throw' } }, steps: [{ op: 'setNext', target: T(B) }] },
  { name: 'set-next-echo-cut', env: { EMBEDDING_MODEL: A }, rag: { indexes: [], served: { '': Array.from({ length: 25 }, (_, i) => `m${i}-${'z'.repeat(130)}`).concat([7, null]) } }, steps: [{ op: 'setNext', target: T(B) }] },
  { name: 'set-next-mixed-paths', env: { EMBEDDING_MODEL: A }, rag: { indexes: [], served: { '': ['C:\\m\\x.bin', '~/w', '../w', 'w.SafeTensors'] } }, steps: [{ op: 'setNext', target: T(B) }] },
  { name: 'set-next-empty-list', env: { EMBEDDING_MODEL: A }, rag: { indexes: [], served: { '': [] } }, steps: [{ op: 'setNext', target: T(B) }] },
  {
    name: 'set-next-replaces', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1', 'i2'], served: { '': [A, B, C], [LOCAL]: [B] } },
    steps: [{ op: 'setNext', target: T(B) }, { op: 'setNext', target: T(C) }, { op: 'setNext', target: T(C, null, '$P1') }, { op: 'setNext', target: T(C, null, '$P1') }, { op: 'setNext', target: T(A, null, '$P1') }, { op: 'setNext', target: T(B, LOCAL) }],
  },
  {
    name: 'clear-next', env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: B, EMBEDDING_NEXT_PROFILE: '$P0' }, rag: { indexes: ['i1', 'i2'] },
    steps: [{ op: 'clearNext' }, { op: 'clearNext' }, { op: 'status' }],
  },
  {
    name: 'switch-flow', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1', 'i2'], served: { '': [A, B, C] }, coverage: { [at('$P0', A, 'i1')]: cov(10, 10), [at('$P0', A, 'i2')]: cov(5, 4), [at('$P0', B, 'i1')]: cov(10, 10), [at('$P0', B, 'i2')]: cov(5, 4), [at('$P0', C, 'i1')]: cov(10, 3), [at('$P0', C, 'i2')]: cov(5, 5) } },
    steps: [
      { op: 'switch' }, { op: 'setNext', target: T(B) }, { op: 'switch' }, { op: 'status' },
      { op: 'setNext', target: T(C) }, { op: 'switch' }, { op: 'switch', force: true }, { op: 'status' },
      { op: 'rollback' }, { op: 'finish' }, { op: 'finish' }, { op: 'rollback' }, { op: 'status' },
    ],
  },
  {
    name: 'switch-displaces-previous', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1'], served: { '': [A, B, C] }, coverageDefault: cov(2, 2) },
    steps: [{ op: 'setNext', target: T(B) }, { op: 'switch' }, { op: 'setNext', target: T(C) }, { op: 'switch' }, { op: 'setNext', target: T(B) }, { op: 'switch' }],
  },
  {
    name: 'switch-unavailable-index', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1', 'i2'], served: { '': [B] }, coverage: { [at('$P0', A, 'i1')]: cov(1, 1), [at('$P0', B, 'i1')]: cov(1, 1) } },
    steps: [{ op: 'setNext', target: T(B) }, { op: 'switch' }],
  },
  {
    name: 'switch-served-gone', env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: B, EMBEDDING_NEXT_PROFILE: '$P0' }, rag: { indexes: ['i1'], served: { '': [A] }, coverageDefault: cov(1, 1) },
    steps: [{ op: 'switch', force: true }],
  },
  {
    name: 'switch-parity-ok', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1'], served: { '': [A], [LOCAL]: [A] }, embed: { '': unit(1), [LOCAL]: unit(1.001) } },
    steps: [{ op: 'setNext', target: T(A, LOCAL) }, { op: 'switch' }, { op: 'status' }, { op: 'rollback' }],
  },
  {
    name: 'switch-parity-fails', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1'], served: { '': [A], [LOCAL]: [A] }, embed: { '': unit(1), [LOCAL]: unit(-3) } },
    steps: [{ op: 'setNext', target: T(A, LOCAL) }, { op: 'switch' }, { op: 'switch', force: true }],
  },
  {
    name: 'switch-parity-throws', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1'], served: { '': [A], [LOCAL]: [A] }, embed: { '': unit(1), [LOCAL]: 'embeddings server refused' } },
    steps: [{ op: 'setNext', target: T(A, LOCAL) }, { op: 'switch' }],
  },
  {
    name: 'switch-parity-width', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1'], served: { '': [A], [LOCAL]: [A] }, embed: { '': unit(1), [LOCAL]: [[1, 2], [1, 2], [1, 2], [1, 2]] } },
    steps: [{ op: 'setNext', target: T(A, LOCAL) }, { op: 'switch' }],
  },
  {
    name: 'switch-parity-zero', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1'], served: { '': [A], [LOCAL]: [A] }, embed: { '': unit(1), [LOCAL]: [[0, 0, 0, 0], [1, 0, 0, 0], [1, 0, 0, 0], [1, 0, 0, 0]] } },
    steps: [{ op: 'setNext', target: T(A, LOCAL) }, { op: 'switch' }],
  },
  {
    name: 'rollback-drops-next', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1', 'i2'], served: { '': [A, B, C] }, coverageDefault: cov(3, 3) },
    steps: [{ op: 'setNext', target: T(B) }, { op: 'switch' }, { op: 'setNext', target: T(C) }, { op: 'rollback' }, { op: 'status' }],
  },
  {
    name: 'drop-fails-logged', env: { EMBEDDING_MODEL: A }, rag: { indexes: ['i1', 'bad'], dropFails: ['bad'], served: { '': [B, C] } },
    steps: [{ op: 'setNext', target: T(B) }, { op: 'setNext', target: T(C) }],
  },
  {
    name: 'refuse-seeded-active', env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: 'http://192.0.2.1' }, rag: { indexes: ['i1'], served: { '': [B] } },
    steps: [{ op: 'refuseSeeded', reasons: { active: 'the address is public' } }, { op: 'status' }, { op: 'setNext', target: T(B) }, { op: 'clearNext' }, { op: 'release', which: 'active' }, { op: 'status' }],
    // Node bug (filed): the locked configuration's set-next and clear-next answer a plain error (500) or
    // the lock check's error instead of the 409 badState; Rust answers 409 badState for set-next.
  },
  {
    name: 'refuse-seeded-transient', env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: 'http://qa-host.invalid', EMBEDDING_NEXT_MODEL: B, EMBEDDING_NEXT_PROFILE: '$P0' }, rag: { indexes: ['i1'] },
    steps: [{ op: 'refuseSeeded', reasons: { active: 'not resolved yet', next: 'not resolved yet' }, transient: true }, { op: 'status' }, { op: 'refuseSeeded', reasons: { next: 'still not' }, transient: true }, { op: 'release', which: 'next' }, { op: 'release', which: 'active' }, { op: 'status' }, { op: 'release', which: 'active' }],
  },
  {
    name: 'refuse-seeded-after-save', env: { EMBEDDING_MODEL: A }, rag: { indexes: [], served: { '': [B] } },
    steps: [{ op: 'setNext', target: T(B) }, { op: 'refuseSeeded', reasons: { active: 'late' } }, { op: 'status' }],
  },
];

async function run(s) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-embedding-'));
  const stateFile = path.join(dir, 'embedding-config.json');
  const scrub = (v) => JSON.parse(JSON.stringify(v ?? null).split(dir).join('<DIR>'));
  const rag = sub(s.rag || { indexes: [] });
  const env = sub(s.env || {});
  if (s.file != null) fs.writeFileSync(stateFile, s.file.replace(/\$P(\d)/g, (_, i) => P[Number(i)]));
  const rec = { drops: [], kicks: 0, logs: [] };
  const log = (line) => rec.logs.push(String(line));
  const snapshot = s.snapshot === undefined ? SNAP : s.snapshot;
  const owner = { kick: () => { rec.kicks += 1; }, snapshot: () => (snapshot ? { ...snapshot } : null) };
  const ragStub = {
    listIndexes: () => rag.indexes.slice(),
    indexCoverage: (entry, t) => { const v = rag.coverage?.[`${t.profile}/${t.model}@${entry}`]; return v === undefined ? (rag.coverageDefault ?? null) : v; },
    dropIndexSlot: (entry, t) => { rec.drops.push([entry, t.profile, t.model]); if ((rag.dropFails || []).includes(entry)) throw Error('qa slot locked'); },
    embed: async (texts, { target }) => { const v = rag.embed?.[target.baseUrl || '']; if (typeof v === 'string') throw Error(v); return v.slice(0, texts.length); },
    servedModels: async (t) => { const v = rag.served?.[t.baseUrl || '']; if (v === 'throw') throw Error('qa probe failed'); return v === undefined ? null : v; },
  };
  const config = loadEmbeddingConfig({ env, nativeOwner: owner, stateFile, log });
  const migration = createEmbeddingMigration({ config, rag: ragStub, nativeOwner: owner, env, log });
  const out = { name: s.name, env, file: s.file == null ? null : fs.readFileSync(stateFile, 'utf8'), rag, snapshot, parseError: !!s.parseError, load: { logs: scrub(rec.logs) }, steps: [] };
  rec.logs = [];
  for (const step0 of s.steps) {
    const step = sub(step0);
    let result;
    try {
      let v;
      if (step.op === 'status') v = migration.status();
      else if (step.op === 'setNext') v = await migration.setNext(step.target);
      else if (step.op === 'clearNext') v = await migration.clearNext();
      else if (step.op === 'switch') v = await migration.switchToNext({ force: step.force === true });
      else if (step.op === 'rollback') v = await migration.rollback();
      else if (step.op === 'finish') v = await migration.finish();
      else if (step.op === 'refuseSeeded') { config.refuseSeeded(step.reasons, { transient: step.transient === true }); v = migration.status(); }
      else if (step.op === 'release') { config.releaseSeeded(step.which); v = migration.status(); }
      result = { ok: v };
    } catch (err) {
      result = { error: { message: String(err?.message || err), status: err?.status ?? null, code: err?.code ?? null, ...(err?.coverage ? { coverage: err.coverage } : {}), ...(err?.parity ? { parity: err.parity } : {}), ...(err?.served ? { served: err.served } : {}) } };
    }
    out.steps.push(scrub({ step, result, drops: rec.drops, kicks: rec.kicks, logs: rec.logs, file: fs.existsSync(stateFile) ? fs.readFileSync(stateFile, 'utf8') : null }));
    rec.drops = []; rec.kicks = 0; rec.logs = [];
  }
  fs.rmSync(dir, { recursive: true, force: true });
  return out;
}

(async () => {
  const scenarios = [];
  for (const s of SCENARIOS) scenarios.push(await run(s));
  process.stdout.write(JSON.stringify({ version: 1, sources: SOURCES, profiles: { default: DEFAULT_PROFILE, known: P }, scenarios }) + '\n');
})().catch((err) => { process.stderr.write(String(err?.stack || err) + '\n'); process.exit(1); });
