#!/usr/bin/env node
'use strict';
// Recorded answers for crates/embedding-migration (track R11), from noevia-core's own modules:
// embedding-config.cjs (loadEmbeddingConfig, normalizeTarget, seedFromEnv), embedding-migration.cjs
// (createEmbeddingMigration with a nativeOwner, as production runs it), embedding-served.cjs
// (checkServed) and routes/embedding.cjs (createEmbeddingRoutes); http.cjs's errorResponse answers an
// error the route throws, as index.cjs's request handler does.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/embedding-migration/tests/record.cjs > crates/embedding-migration/tests/fixtures/node-embedding-migration.v1.json
//
// Every input is synthetic and QA-named (.invalid hosts, qa-embed-* models). A scenario boots a fresh
// config from a synthetic environment and an optional saved file (a relative path under a temporary
// working directory, so messages naming it are the same on replay), then runs steps: requests through
// the real route, changes to the stub world, restarts, start-up refusals. Recorded per step: the answer,
// every stub call in order with its arguments, and the saved file afterwards. The stubs (the config
// lock, the native worker, rag, the address check, the Diary's fetchJson, the audit log, the embedding
// service, resolveFile) answer from a plain JSON "world" the replay reads the same way. The source files'
// sha256 are checked, so a changed Node file needs a fresh recording. Node's console goes to stderr.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) { process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n'); process.exit(2); }
const root = path.resolve(checkout);
const SOURCES = {
  'server/embedding-config.cjs': '03d89968f9cf77b124168f2c741b91e1ffaf5580a0dfe4a5ad299232546a29e7',
  'server/embedding-config-lock.cjs': 'dddeed10606f3a782a51278151651ba7c693b2816746cb115f400a81b43622d2',
  'server/embedding-profile.cjs': 'fa349dd5909a912335bb81e81b1e670e1ed774cbca626da30853e24c3743b740',
  'server/embedding-migration.cjs': 'b97076fc242eaf337f1e16813942fe5edc342e7c6594787c00aa4af752c24bdd',
  'server/embedding-served.cjs': '9ab1c0d06c60d53beef408a887c5ca4f090451e91457ae50f89ff7c029054f3b',
  'server/routes/embedding.cjs': 'ae34b6be30f2b8880a5c6c0c141ed9d1e709e87ef92202bb3cc0e79465db9854',
  'server/http.cjs': 'd9bc3d67258f4adcbd10bfaaf9e6674086110fbba079fe8f6678340d3a0080e0',
};
for (const [rel, want] of Object.entries(SOURCES)) {
  const got = crypto.createHash('sha256').update(fs.readFileSync(path.join(root, rel))).digest('hex');
  if (got !== want) { process.stderr.write(`source hash mismatch for ${rel}: ${got}\n`); process.exit(1); }
}
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => process.stderr.write(`${a.map(String).join(' ')}\n`);
const req = (rel) => require(path.join(root, 'server', rel));
const lockModule = req('embedding-config-lock.cjs');
const { loadEmbeddingConfig, normalizeTarget, seedFromEnv } = req('embedding-config.cjs');
const { createEmbeddingMigration } = req('embedding-migration.cjs');
const { checkServed } = req('embedding-served.cjs');
const { createEmbeddingRoutes } = req('routes/embedding.cjs');
const { errorResponse } = req('http.cjs');

const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));
let calls = [];
let world = null;
const log = (...entry) => calls.push(clone(entry));

// ── Stubs: each logs its call, then answers from the world ─────────────────────────────────────
lockModule.withFlock = (file, action) => new Promise((resolve, reject) => {
  log('lock');
  if (world.lockFail) { reject(Error(world.lockFail)); return; }
  let result, failure = null;
  try { result = action(); } catch (e) { failure = e; }
  log('unlock');
  if (failure) reject(failure); else resolve(result);
});
const nativeOwner = { kick() { log('kick'); }, snapshot() { log('snapshot'); return clone(world.snapshot); } };
const rag = {
  listIndexes() { log('indexes'); return world.indexes.slice(); },
  indexCoverage(entry, t) { log('coverage', entry, t); const c = world.coverage[`${entry}|${t.profile}|${t.model}`]; return c ? { ...c } : null; },
  dropIndexSlot(entry, t) { log('dropSlot', entry, t); if (world.dropFails.includes(entry)) throw Error('qa drop failed'); },
  async embed(texts, o) {
    log('embed', texts, o.kind, o.target);
    const v = world.vectors[o.target.baseUrl || ''];
    if (typeof v === 'string') throw Error(v);
    if (!Array.isArray(v)) throw Error('no vectors');
    return clone(v);
  },
  async servedModels(t) { log('servedModels', t.baseUrl, t.model); const ids = world.served[t.baseUrl || '']; if (!Array.isArray(ids)) throw Error('qa unreachable'); return ids.slice(); },
};
const checkAddress = async (url) => { log('checkAddress', url); const reason = world.public[url]; return reason ? { ok: false, reason, transient: false } : { ok: true }; };
const fetchJson = async (url, init, timeout) => {
  log('diary', url, init.method, init.body ?? null, timeout);
  const reply = world.diaryQueue.length ? world.diaryQueue.shift() : world.diary[init.method];
  if (!reply) throw Error('no reply');
  if ('throw' in reply) throw Error(reply.throw);
  return { ok: reply.ok, status: reply.status, body: reply.body === undefined ? undefined : clone(reply.body) };
};
const authService = { audit(action, user, target, detail) { log('audit', action, user, target, detail); } };
const svc = () => world.service;
const service = {
  get baseUrl() { return svc().baseUrl; },
  async refresh() { log('serviceRefresh'); },
  managed() { log('serviceManaged'); return !!svc().managed; },
  enabled() { log('serviceEnabled'); return svc().enabled; },
  status() { log('serviceStatus'); return clone(svc().status); },
  isAt(address) { log('serviceIsAt', address); return svc().at.some((a) => a === address); },
  fileOf(model) { log('serviceFileOf', model); return svc().known[model] ?? null; },
  async serve(models) {
    log('serviceServe', models);
    const f = svc().serveFail.length ? svc().serveFail.shift() : null;
    if (f) throw Object.assign(Error(f.message), f.status === undefined ? {} : { status: f.status }, f.code === undefined ? {} : { code: f.code });
  },
  async waitLoaded(names, o) { log('serviceWaitLoaded', names, o.timeoutMs); return svc().loaded.length ? svc().loaded.shift() : true; },
};
const resolveFile = async (model) => { log('resolveFile', model); const f = svc().installed[model]; if (f === '!throw') throw Error('qa'); return f ?? null; };

// ── The world and the scenarios ─────────────────────────────────────────────────────────────────
const SVC = 'http://qa-embed.invalid:8080';
const OTHER = 'http://qa-other.invalid:9090';
const PUBLIC = 'http://qa-public.invalid:8080';
const DIARY_EMBED = 'http://qa-diary-embed.invalid:8081';
const A = 'qa-embed-a', B = 'qa-embed-b', C = 'qa-embed-c';
const GEMMA = 'embeddinggemma-2';
const ASTRAL = `qa-${'\u{1F600}'.repeat(70)}`; // 143 UTF-16 units: shown cut at 120 halves a pair
const UNIT = [[1, 0, 0], [0, 1, 0], [0, 0, 1], [0.5, 0.5, 0]];
const NUDGED = [[0.99, 0.01, 0], [0, 1, 0], [0, 0, 1], [0.5, 0.5, 0]];
const OFF = [[0, 1, 0], [0, 1, 0], [0, 0, 1], [0.5, 0.5, 0]];
const diaryStatus = (active, next = null, previous = null) => ({ ok: true, status: 200, body: { active, next, previous, source: 'saved' } });
const dt = (model, base_url = DIARY_EMBED, profile = 'nomic-v1') => ({ model, profile, base_url });
const snap = { protocol: 'rust-embedding-worker-v1', running: false, lastPass: { at: 1, embedded: 3, failed: 0 }, nextServed: true, nextHeld: null, configValid: true, configInvalid: null };
function defaults() {
  return {
    indexes: ['qa-u1/p1', 'qa-u2/p1'], coverage: {}, served: { '': [A, B, C], [SVC]: [A, B, C], [OTHER]: [A, B], [DIARY_EMBED]: [A, B, C] },
    vectors: { '': UNIT, [SVC]: UNIT, [OTHER]: OFF }, public: { [PUBLIC]: 'qa-public.invalid resolves to a public address' },
    dropFails: [], lockFail: null, snapshot: snap, diaryBase: 'http://qa-diary.invalid:8090/',
    diary: { GET: diaryStatus(dt(A)), POST: { ok: true, status: 200, body: { ok: true, qa: 'diary' } } }, diaryQueue: [],
    service: { managed: false, enabled: true, baseUrl: SVC, at: [SVC, `${SVC}/v1`], known: {}, installed: {}, status: null, serveFail: [], loaded: [] },
  };
}
const cov = (model, chunks, vectors, profile = 'nomic-v1', indexes = ['qa-u1/p1', 'qa-u2/p1']) => Object.fromEntries(indexes.map((i) => [`${i}|${profile}|${model}`, { chunks, vectors }]));
const post = (body, extra = {}) => ({ post: JSON.stringify(body), ...extra });
const GET = { get: true };
const setNext = (target, more = {}) => post({ action: 'set-next', target, ...more });
const act = (action, more = {}) => post({ action, ...more });
const P = { scope: 'projects' };
const ALL_ACTIONS = ['set-next', 'clear-next', 'switch', 'rollback', 'finish'];

const scenarios = [];
const add = (name, spec) => scenarios.push({ name, env: {}, file: undefined, world: {}, ...spec });

// Seeding and the saved file.
add('env-empty', { steps: [GET] });
add('env-embed-model', { env: { EMBED_MODEL: A }, steps: [GET] });
add('env-both-models', { env: { EMBEDDING_MODEL: ` ${B} `, EMBED_MODEL: A, EMBEDDING_PROFILE: GEMMA }, steps: [GET] });
add('env-blank-model', { env: { EMBEDDING_MODEL: '  ', EMBED_MODEL: A, EMBEDDING_PROFILE: ' ' }, steps: [GET] });
add('env-next-same', { env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: A }, steps: [GET] });
add('env-next-moved', { env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: A, EMBEDDING_NEXT_BASE_URL: SVC }, steps: [GET, act('switch', P), GET] });
add('env-next-profile', { env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: B, EMBEDDING_NEXT_PROFILE: GEMMA, EMBEDDING_NEXT_BASE_URL: OTHER }, steps: [GET] });
add('env-next-base-only', { env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_BASE_URL: SVC, EMBEDDING_NEXT_PROFILE: GEMMA }, steps: [GET] });
add('env-unknown-profile', { env: { EMBEDDING_MODEL: A, EMBEDDING_PROFILE: 'qa-unknown' }, steps: [GET, act('clear-next', P), { restart: true }, GET, setNext({ model: B }, P)] });
add('env-trailing-slash', { env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: `${SVC}//` }, steps: [GET, act('clear-next', P), { restart: true }, GET] });
add('env-bad-url', { env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: 'qa not a url' }, steps: [GET, act('clear-next', P), { restart: true }, GET] });
for (const [duty, batch] of [['0.25', '7.5'], [' 2 ', '-3'], ['', ''], ['abc', '0x10'], ['1e-3', '8.5'], ['Infinity', '2.49'], ['0b1', '+4'], ['.5', '16.5']]) {
  add(`env-pacing-${duty}-${batch}`, { env: { EMBEDDING_MODEL: A, EMBEDDING_REINDEX_DUTY: duty, EMBEDDING_REINDEX_BATCH: batch }, steps: [GET] });
}
const good = (s) => JSON.stringify({ version: 1, ...s }, null, 2);
const files = {
  'not-json': '{', empty: '', 'version-2': '{"version":2,"active":{"model":"qa-embed-a"}}', 'version-string': '{"version":"1","active":{"model":"qa-embed-a"}}',
  null: 'null', array: '[]', number: '7', 'no-active': '{"version":1}', 'bad-next-url': '{"version":1,"active":{"model":"qa-embed-a"},"next":{"model":"qa-embed-b","baseUrl":"ftp://qa.invalid"}}',
  'bad-profile': '{"version":1,"active":{"model":"qa-embed-a","profile":"qa-other"}}', 'bad-previous': '{"version":1,"active":{"model":"qa-embed-a"},"previous":{"model":"qa x"}}',
  'coerce-profile': '{"version":1,"active":{"model":"qa-embed-a","profile":{"toString":1}}}', bom: `\ufeff${good({ active: { model: A } })}`,
  'falsy-next': '{"version":1,"active":{"model":"qa-embed-a","baseUrl":"http://qa-embed.invalid:8080/"},"next":0,"previous":""}',
  'full': good({ active: { baseUrl: SVC, model: B, profile: GEMMA }, next: { baseUrl: null, model: C }, previous: { model: A } }),
  'astral': good({ active: { model: ASTRAL } }), 'loose': '{"version":1.0,"active":{"model":" qa-embed-a ","profile":" ","baseUrl":" HTTP://QA-Embed.invalid:8080/v1/ "},"extra":true}',
};
for (const [name, file] of Object.entries(files)) {
  add(`file-${name}`, { env: { EMBEDDING_MODEL: C }, file, world: { coverage: cov(A, 4, 4) }, steps: [GET, setNext({ model: B }, P), act('clear-next', P), act('switch', P), act('rollback', P), act('finish', P), GET, act('finish', { scope: 'all' })] });
}
add('file-deleted', { env: { EMBEDDING_MODEL: C }, file: files.full, steps: [GET, { writeFile: null }, { restart: true }, GET, { writeFile: '{"version":1,' }, { restart: true }, GET, { writeFile: files.full }, { restart: true }, GET] });

// The migration's transitions.
add('flow-full', {
  env: { EMBEDDING_MODEL: A }, world: { coverage: { ...cov(A, 10, 10), ...cov(B, 10, 6) } },
  steps: [GET, setNext({ baseUrl: SVC, model: B }, P), GET, act('switch', P), { world: { coverage: { ...cov(A, 10, 10), ...cov(B, 10, 10) } } }, act('switch', P), GET,
    act('rollback', P), act('switch', P), act('finish', P), act('finish', P), act('rollback', P), act('switch', P), { restart: true }, GET],
});
add('set-next-replace', { env: { EMBEDDING_MODEL: A }, steps: [setNext({ model: B }, P), setNext({ model: C, profile: GEMMA }, P), setNext({ model: C, profile: GEMMA }, P), setNext({ model: C, baseUrl: SVC, profile: GEMMA }, P), setNext({ model: A, baseUrl: SVC }, P), act('clear-next', P), act('clear-next', P)] });
add('set-next-after-switch', { env: { EMBEDDING_MODEL: A }, world: { coverage: cov(B, 1, 1) }, steps: [setNext({ model: B }, P), act('switch', P), setNext({ model: A }, P), setNext({ model: C }, P), act('rollback', P), setNext({ model: B }, P), act('switch', { ...P, force: true }), act('finish', P)] });
add('already-active', { env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: SVC }, steps: [setNext({ model: A, baseUrl: `${SVC}/` }, P), setNext({ model: A }, P), setNext({ model: A, baseUrl: SVC, profile: GEMMA }, P)] });
add('not-served', {
  env: { EMBEDDING_MODEL: A },
  world: { served: { '': [A, B], [SVC]: ['/models/qa-embed.gguf', 'C:\\qa\\b.bin'], [OTHER]: Array.from({ length: 25 }, (_, i) => `qa-${'m'.repeat(i * 6)}`), [DIARY_EMBED]: [`${B.toUpperCase()}:latest`] } },
  steps: [setNext({ model: C }, P), setNext({ model: B, baseUrl: SVC }, P), setNext({ model: B, baseUrl: OTHER }, P), setNext({ model: B, baseUrl: 'http://qa-gone.invalid' }, P),
    setNext({ model: B, baseUrl: DIARY_EMBED }, P), setNext({ model: ASTRAL, baseUrl: OTHER }, P), setNext({ model: `  ${B}:latest `, baseUrl: DIARY_EMBED }, P)],
});
add('switch-not-served', { env: { EMBEDDING_MODEL: A }, world: { coverage: cov(B, 2, 2) }, steps: [setNext({ model: B, baseUrl: SVC }, P), { world: { served: { [SVC]: [A] } } }, act('switch', { ...P, force: true }), { world: { served: {} } }, act('switch', P)] });
const parity = (name, vectors, force) => add(`parity-${name}`, {
  env: { EMBEDDING_MODEL: A }, steps: [setNext({ model: A, baseUrl: SVC }, P), { world: { vectors: { '': UNIT, [SVC]: vectors } } }, act('switch', P), ...(force ? [act('switch', { ...P, force: true })] : []), GET],
});
parity('equal', UNIT); parity('nudged', NUDGED); parity('off', OFF, true); parity('throws', 'qa embed refused', true); parity('empty-message', '', false);
parity('none', [], false); parity('short', UNIT.slice(0, 2), false); parity('zero', [[0, 0, 0], [0, 1, 0], [0, 0, 1], [1, 1, 1]], false);
parity('width', [[1, 0], [0, 1, 0], [0, 0, 1], [0.5, 0.5, 0]], false); parity('missing', null, true);
add('parity-first-throws', { env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: A, EMBEDDING_NEXT_BASE_URL: SVC }, world: { vectors: { '': 'qa engine down' } }, steps: [act('switch', P)] });
add('not-ready', {
  env: { EMBEDDING_MODEL: A }, world: { indexes: ['qa-u1/p1', 'qa-u2/p1', 'qa-u3/p2'], coverage: { ...cov(A, 5, 5), ...cov(B, 5, 5, 'nomic-v1', ['qa-u1/p1']) } },
  steps: [setNext({ model: B }, P), act('switch', P), { world: { coverage: { ...cov(A, 5, 4), ...cov(B, 5, 4) } } }, act('switch', P), { world: { indexes: [] } }, GET, act('switch', P)],
});
add('lock-fails', { env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: B }, world: { lockFail: 'Embedding config lock timed out' }, steps: [setNext({ model: C }, P), act('clear-next', P), act('switch', { ...P, force: true }), act('rollback', P), act('finish', P), GET] });
add('drop-fails', { env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: B }, world: { dropFails: ['qa-u1/p1'] }, steps: [act('clear-next', P), GET] });
add('no-snapshot', { env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: B }, world: { snapshot: null }, steps: [GET, { world: { snapshot: { running: true, nextHeld: { code: 'modelNotServed', error: 'qa' }, configValid: false, configInvalid: 'qa invalid', extra: 1 } } }, GET, { world: { snapshot: {} } }, GET] });
add('refuse-active', { env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: PUBLIC, EMBEDDING_NEXT_MODEL: B }, steps: [{ refuse: { active: 'qa: the address is public' } }, GET, setNext({ model: C }, P), setNext({ model: A }, P), act('clear-next', P), act('switch', P), act('rollback', P), act('finish', P), { refuse: { active: 'qa: again', next: 'qa next' } }, GET] });
add('refuse-next', { env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: B, EMBEDDING_NEXT_BASE_URL: PUBLIC }, steps: [{ refuse: { next: 'qa: next refused' } }, GET, act('switch', P), setNext({ model: B }, P)] });
add('refuse-after-save', { env: { EMBEDDING_MODEL: A }, steps: [act('clear-next', P), { refuse: { active: 'qa: late' } }, GET] });
// releaseSeeded: only a transient refusal is taken back, and only while nothing was saved.
const T = { transient: true };
add('release-transient', {
  env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: SVC, EMBEDDING_NEXT_MODEL: B, EMBEDDING_NEXT_BASE_URL: OTHER },
  steps: [{ release: 'active' }, { refuse: { active: 'qa: not yet', next: 'qa: next not yet' }, ...T }, GET, setNext({ model: C }, P), { release: 'next' }, GET, { release: 'next' },
    { release: 'active' }, GET, setNext({ model: C }, P), { release: 'active' }, { release: 'qa' }],
});
add('release-not-transient', {
  env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: SVC, EMBEDDING_NEXT_MODEL: B, EMBEDDING_NEXT_BASE_URL: OTHER },
  steps: [{ refuse: { active: 'qa: refused', next: 'qa: next refused' } }, { release: 'active' }, { release: 'next' }, GET, { refuse: { next: 'qa: next later' }, ...T }, { release: 'next' }, GET],
});
add('release-then-refuse', {
  env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: SVC }, steps: [{ refuse: { active: 'qa: first' }, ...T }, { refuse: { active: 'qa: for good' } }, { release: 'active' }, GET, act('clear-next', P)],
});
add('release-after-change', {
  env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: B, EMBEDDING_NEXT_BASE_URL: OTHER },
  steps: [{ refuse: { next: 'qa: next not yet' }, ...T }, setNext({ model: C }, P), { release: 'next' }, GET, act('clear-next', P), { release: 'next' }, GET],
});
add('release-saved-file', {
  env: { EMBEDDING_MODEL: C }, file: files.full, steps: [{ refuse: { active: 'qa: seeded', next: 'qa: next' }, ...T }, { release: 'active' }, { release: 'next' }, GET],
});
add('release-after-restart', {
  env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: B, EMBEDDING_NEXT_BASE_URL: OTHER }, steps: [{ refuse: { next: 'qa: next not yet' }, ...T }, { restart: true }, { release: 'next' }, GET],
});

// Target validation through set-next (every name is served, so normalizeTarget decides).
const odd = [B, `${B}\u007f`, `${B}\u0085`, `${B}\ufeff`, `qa\u00a0b`, `qa\u0000`, 'x'.repeat(200), 'x'.repeat(201), '\u{1F600}'.repeat(100), '\u{1F600}'.repeat(101), 'qa x', ` ${C} `];
add('targets-models', { env: { EMBEDDING_MODEL: A }, world: { served: { '': odd.map((m) => m.trim()) } }, steps: odd.map((model) => setNext({ model }, P)) });
const rawTargets = ['{"action":"set-next","scope":"projects","target":[]}', '{"action":"set-next","scope":"projects","target":{"model":5}}', '{"action":"set-next","scope":"projects","target":{"model":["qa-embed-b"]}}',
  '{"action":"set-next","scope":"projects","target":{"model":"qa-embed-b","profile":{"toString":1}}}', '{"action":"set-next","scope":"projects","target":{"model":"qa-embed-b","profile":7}}',
  '{"action":"set-next","scope":"projects","target":{"model":"qa-embed-b","profile":null}}', '{"action":"set-next","scope":"projects","target":{"model":"qa-embed-b","profile":"__proto__"}}',
  '{"action":"set-next","scope":"projects","target":{"model":"qa-embed-b","profile":"hasOwnProperty"}}', '{"action":"set-next","scope":"projects","target":{"model":"qa-\\ud800b"}}',
  `{"action":"set-next","scope":"projects","target":{"model":"qa-embed-b","profile":"${'\u{1F600}'.repeat(200)}"}}`, '{"action":"set-next","scope":"projects","target":{"model":"qa-embed-b","baseUrl":5}}'];
add('targets-raw', { env: { EMBEDDING_MODEL: A }, world: { served: { '': ['5', B, 'qa-\ud800b'], 5: [B] } }, steps: rawTargets.map((raw) => ({ post: raw })) });
const urls = ['http://qa.invalid', 'https://qa.invalid:8443/v1/', 'HTTP://QA.invalid', 'ftp://qa.invalid', 'http://user@qa.invalid', 'http://:pw@qa.invalid', 'http://@qa.invalid',
  'http://qa.invalid/?', 'http://qa.invalid/?a=1', 'http://qa.invalid/#', 'http://qa.invalid/#x', 'qa.invalid:8080', '//qa.invalid', 'http://', 'http:qa.invalid', 'http://qa .invalid',
  'http://[::1]:8080', 'http://10.0.0.5:99999', 'http://qa.invalid:8080////', 'javascript:alert(1)', 'http://qa.invalid\\v1', 'http://%41.invalid', 'http://1.2.3.4.5',
  'http://b\u00fccher.invalid/', 'https://qa.invalid:443', 'http://qa.invalid:0/x', 'http://u:@qa.invalid', 'http://qa.invalid/ x', 'http://qa.invalid\t/', 'file:///qa', 'data:text/plain,qa'];
add('targets-urls', { env: { EMBEDDING_MODEL: A }, world: { served: Object.fromEntries(urls.map((u) => [u.trim(), [B]])) }, steps: urls.map((baseUrl) => setNext({ model: B, baseUrl }, P)) });

// The route: refusals, the address and served checks, the Diary half and its mixes.
add('route-refusals', {
  env: { EMBEDDING_MODEL: A },
  steps: [{ get: true, as: 'anon' }, { get: true, as: 'member' }, { post: '{}', as: 'member' }, { method: 'PUT' }, { method: 'HEAD' }, { post: '{' }, { post: '' }, { post: 'null' }, { post: '[]' }, { post: '"switch"' },
    act(''), act('SWITCH'), act('switch', { scope: 'none' }), act('finish', { scope: ['diary'] }), act('set-next'), act('set-next', { target: 'x' }), act('set-next', { target: null }), act('finish', { scope: 0 })],
});
add('route-addresses', {
  env: { EMBEDDING_MODEL: A },
  steps: [setNext({ model: B, baseUrl: PUBLIC }), setNext({ model: B, baseUrl: SVC, diaryBaseUrl: PUBLIC }), setNext({ model: B, baseUrl: SVC }), setNext({ model: B, baseUrl: ` ${SVC} `, diaryBaseUrl: ` ${SVC}` }),
    setNext({ model: B }), setNext({ model: B, diaryBaseUrl: '' }), setNext({ model: B, diaryBaseUrl: DIARY_EMBED }), setNext({ model: B, baseUrl: OTHER, diaryBaseUrl: DIARY_EMBED, profile: GEMMA }), setNext({ model: B }, { scope: 'diary' }),
    setNext({ model: C, baseUrl: SVC, diaryBaseUrl: null }, { scope: 'diary' }), setNext({ model: ` ${C}`, diaryBaseUrl: OTHER }, { scope: 'diary' }), setNext({ model: 0, baseUrl: SVC })],
});
const long = `qa ${'\u{1F600}'.repeat(160)} refused`;
const diaryReplies = {
  refused: { ok: false, status: 409, body: { error: 'qa diary refused', code: 'notReady' } }, detail: { ok: false, status: 500, body: { detail: 'qa detail', code: '' } },
  empty: { ok: false, status: 503, body: null }, text: { ok: false, status: 502, body: 'qa gateway' }, numbered: { ok: false, status: 400, body: { error: 42, code: 7 } },
  coerce: { ok: false, status: 400, body: { error: { toString: 1 } } }, long: { ok: false, status: 422, body: { error: long } }, throws: { throw: 'connect ECONNREFUSED qa-diary.invalid' },
  'throws-long': { throw: `qa ${'x'.repeat(130)}` }, 'throws-empty': { throw: '' }, 'ok-null': { ok: true, status: 200, body: null }, 'ok-error': { ok: true, status: 200, body: { error: 'qa soft', status: 418 } },
  'ok-error-odd': { ok: true, status: 200, body: { error: 'qa soft', status: 700 } }, 'ok-text': { ok: true, status: 200, body: 'qa' }, 'ok-none': { ok: true, status: 204 },
};
for (const [name, reply] of Object.entries(diaryReplies)) {
  add(`diary-${name}`, {
    env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: B }, world: { diary: { GET: reply, POST: reply } },
    steps: [GET, act('clear-next'), act('switch'), act('switch', { scope: 'diary', force: true, target: { model: C, baseUrl: SVC, diaryBaseUrl: '' } }), act('rollback', { target: [] }), setNext({ model: B, baseUrl: SVC, profile: GEMMA }, { scope: 'all', force: 'yes' })],
  });
}
add('diary-not-configured', { env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: B }, world: { diaryBase: '' }, steps: [GET, act('finish'), act('finish', { scope: 'diary' }), act('clear-next')] });
add('diary-base-slash', { env: { EMBEDDING_MODEL: A }, world: { diaryBase: '/' }, steps: [GET, act('finish', { scope: 'diary' })] });

// The embedding service: service-off, service-switch, the sync after a change, servedSet.
const managed = (over = {}) => ({ ...defaults().service, managed: true, known: { [A]: 'qa/a.gguf', [B]: 'qa/b.gguf', [C]: 'qa/c.gguf' }, installed: { [A]: 'qa/a.gguf', [B]: 'qa/b.gguf', [C]: 'qa/c.gguf' },
  status: { managed: true, enabled: true, models: [{ model: A, file: 'qa/a.gguf' }, { model: 'qa-x' }, null, { model: B, file: 'qa/b.gguf', extra: 1 }], stale: false }, ...over });
add('service-off', {
  env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: SVC, EMBEDDING_NEXT_MODEL: B, EMBEDDING_NEXT_BASE_URL: SVC }, world: { coverage: cov(B, 1, 1) },
  steps: [act('service-off'), { world: { service: managed() } }, act('service-off'), act('switch', P), act('service-off'), act('finish', P), { world: { diary: { GET: { throw: 'qa down' } } } }, act('service-off'),
    { world: { diary: { GET: diaryStatus(dt(A, ''), dt(B)) } } }, act('service-off'), { world: { diary: { GET: diaryStatus(dt(A, ''), null, dt(C)) } } }, act('service-off'),
    { world: { diary: { GET: diaryStatus(null) } } }, act('service-off'), { world: { diary: { GET: diaryStatus(dt(A)) }, service: managed({ serveFail: [{ message: 'qa refused', status: 400, code: 'serviceRefused' }, { message: 'qa down' }] }) } },
    act('service-off'), act('service-off'), act('service-off'), { world: { diaryBase: '' } }, { restart: true }, act('service-off'), GET],
});
add('service-switch-refusals', {
  env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: SVC }, world: { service: managed({ installed: { [A]: 'qa/a.gguf', [B]: 'qa/b.gguf', [C]: '!throw', 'qa-empty': '' } }) },
  steps: [act('service-switch'), act('service-switch', { model: '  ' }), act('service-switch', { model: 'qa-missing' }), act('service-switch', { model: C }), act('service-switch', { model: 'qa-empty' }),
    act('service-switch', { model: A }), { world: { diary: { GET: { throw: 'qa down' } } } }, act('service-switch', { model: B }), { world: { diary: { GET: diaryStatus(dt(C, SVC)) } } },
    act('service-switch', { model: B }), { world: { diary: { GET: diaryStatus(dt(C, ''), dt(B, '')) } } }, act('service-switch', { model: B }), { world: { diary: { GET: diaryStatus(dt(C, SVC), dt(B, `${SVC}/v1`)) } } },
    act('service-switch', { model: B }), { world: { diary: { GET: diaryStatus(dt(A, SVC)) } } }, act('service-switch', { model: B, profile: GEMMA })],
});
add('service-switch-busy', { env: { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: B }, world: { service: managed(), coverage: cov(B, 1, 1) }, steps: [act('service-switch', { model: C }), act('switch', P), act('service-switch', { model: C }), act('service-switch', { scope: 'nonsense', model: C })] });
add('service-switch-load', {
  env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: `${SVC}/v1` }, world: { service: managed({ loaded: [false], serveFail: [null, null, { message: 'qa model manager refused', status: 503, code: 'serviceUnavailable' }] }) },
  steps: [act('service-switch', { model: B }), act('service-switch', { model: B }), GET, { world: { service: managed({ serveFail: [{ message: 'qa down' }] }) } }, act('service-switch', { model: C, profile: GEMMA })],
});
add('service-switch-from-off', {
  env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: SVC }, world: { service: managed({ enabled: false }) },
  steps: [act('service-switch', { model: A }), act('service-switch', { model: A, profile: GEMMA }), act('clear-next', P), { world: { service: managed({ enabled: null }) } }, act('service-switch', { model: A, profile: 'nomic-v1' }),
    act('service-switch', { model: A, profile: 'qa-bad' }), act('clear-next', P), { world: { service: managed({ enabled: false, loaded: [false] }) } }, act('service-switch', { model: B }), act('service-switch', { model: C })],
});
add('service-switch-elsewhere', { env: { EMBEDDING_MODEL: A }, world: { service: managed({ baseUrl: null }) }, steps: [act('service-switch', { model: B }), { world: { service: managed() } }, act('service-switch', { model: B, profile: 7 })] });
add('service-sync', {
  env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: SVC, EMBEDDING_NEXT_MODEL: B, EMBEDDING_NEXT_BASE_URL: SVC }, world: { service: managed(), coverage: cov(B, 1, 1), diary: { GET: diaryStatus(dt(C, ''), null, { model: B, profile: GEMMA, baseUrl: SVC, base_url: OTHER }), POST: diaryReplies.refused } },
  steps: [act('switch'), act('rollback', P), act('switch', P), { world: { diary: { GET: { throw: 'qa down' } } } }, act('finish', P), setNext({ model: C, baseUrl: SVC }, P), { world: { diary: { GET: diaryStatus(dt(A, SVC), dt(C, SVC), dt(B, SVC)) }, service: managed({ known: { [A]: 'qa/a.gguf', [C]: '' } }) } },
    act('clear-next', P), { world: { service: managed({ enabled: false }) } }, act('finish', P), setNext({ model: B, baseUrl: SVC }, P), { world: { service: managed({ serveFail: [{ message: 'qa sync fails' }] }) } }, act('clear-next', P),
    { world: { diaryBase: '' } }, { restart: true }, setNext({ model: C, baseUrl: SVC }, P), act('clear-next', P)],
});
// A truthy status.models that is not an array: `.filter` throws before anything is served.
const models = (m, over = {}) => ({ world: { service: managed({ status: { managed: true, enabled: true, models: m }, loaded: [false], ...over }) } });
add('service-switch-odd-models', {
  env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: SVC },
  steps: [models({}), act('service-switch', { model: B }), models('x'), act('service-switch', { model: B }), models(7), act('service-switch', { model: B }), models(true), act('service-switch', { model: B }),
    models(''), act('service-switch', { model: B }), models({}, { enabled: false }), act('service-switch', { model: B }), GET],
});
// A Diary target whose address String() cannot convert: servedSet throws (the sync after a change, a service-switch).
const unreadable = { toString: 1 };
const diaryGets = (active, next = null, previous = null) => ({ world: { diary: { GET: diaryStatus(active, next, previous), POST: { ok: true, status: 200, body: { ok: true } } } } });
add('served-set-throws', {
  env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: SVC, EMBEDDING_NEXT_MODEL: C, EMBEDDING_NEXT_BASE_URL: SVC }, world: { service: managed(), diary: { GET: diaryStatus(dt(B, unreadable)) } },
  steps: [act('clear-next', P), GET, act('service-switch', { model: C }), diaryGets(dt(A, SVC), null, dt(C, unreadable)), act('service-switch', { model: C }),
    diaryGets(dt(B, SVC), null, dt(C, unreadable)), act('service-switch', { model: C }), diaryGets(dt(B, [unreadable])), act('service-switch', { model: C }),
    diaryGets(dt(B, { valueOf: 1 })), act('service-switch', { model: C }), diaryGets(dt(B, unreadable)), act('finish', { scope: 'diary' }), act('clear-next', P), GET],
});
add('service-sync-full', {
  env: { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: SVC }, world: { service: managed({ at: [SVC, DIARY_EMBED, null] }), diary: { GET: diaryStatus(dt(B), dt(C), dt(A)) } },
  steps: [act('service-switch', { model: C }), act('finish', P), { world: { diary: { GET: diaryStatus(dt(A), dt(A, DIARY_EMBED, GEMMA), { model: 5, base_url: SVC }) } } }, act('finish', P),
    { world: { diary: { GET: diaryStatus(dt(B), null, dt(C)) } } }, act('service-switch', { model: B }), act('service-switch', { model: C })],
});

// Seeded random sequences over everything above.
let seed = 0x2026_1114;
function rand() { seed = (seed + 0x6d2b79f5) | 0; let t = Math.imul(seed ^ (seed >>> 15), 1 | seed); t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t; return ((t ^ (t >>> 14)) >>> 0) / 4294967296; }
const pick = (list) => list[Math.floor(rand() * list.length)];
const maybe = (p, v) => (rand() < p ? v : undefined);
function randomBody() {
  const action = pick([...ALL_ACTIONS, ...ALL_ACTIONS, 'service-switch', 'service-off', 'qa']);
  const body = { action, scope: pick(['all', 'projects', 'projects', 'diary', undefined]) };
  if (action === 'set-next' || rand() < 0.1) body.target = { model: pick([A, B, C, ` ${B}`, 'qa x', ASTRAL]), baseUrl: pick(['', SVC, `${SVC}/`, OTHER, PUBLIC, undefined, 'ftp://qa.invalid']), profile: maybe(0.3, pick([GEMMA, 'qa-bad'])), diaryBaseUrl: maybe(0.4, pick([DIARY_EMBED, '', SVC])) };
  if (action === 'service-switch') { body.model = pick([A, B, C, '']); body.profile = maybe(0.3, GEMMA); }
  if (rand() < 0.3) body.force = pick([true, 'true']);
  return body;
}
function randomWorld() {
  const models = [A, B, C, ASTRAL];
  const coverage = {};
  for (const m of models) for (const i of ['qa-u1/p1', 'qa-u2/p1', 'qa-u3/p1']) if (rand() < 0.7) coverage[`${i}|${pick(['nomic-v1', 'nomic-v1', GEMMA])}|${m}`] = { chunks: 8, vectors: pick([8, 8, 5, 0]) };
  const served = (p) => models.filter(() => rand() < p);
  return {
    indexes: ['qa-u1/p1', 'qa-u2/p1', 'qa-u3/p1'].slice(0, Math.floor(rand() * 4)), coverage, served: { '': served(0.8), [SVC]: served(0.8), [OTHER]: served(0.6), [DIARY_EMBED]: served(0.9) },
    vectors: { '': UNIT, [SVC]: pick([UNIT, NUDGED, OFF]), [OTHER]: pick([UNIT, 'qa refused']) }, lockFail: maybe(0.05, 'Embedding config lock unavailable'),
    diary: { GET: pick([diaryStatus(dt(A)), diaryStatus(dt(B, ''), dt(C)), diaryReplies.throws]), POST: pick(Object.values(diaryReplies).concat([{ ok: true, status: 200, body: { ok: true } }])) },
    service: rand() < 0.5 ? managed({ enabled: pick([true, true, false, null]), loaded: [pick([true, true, false])] }) : defaults().service,
  };
}
for (let n = 0; n < 70; n++) {
  const env = { EMBEDDING_MODEL: pick([A, B]), ...(rand() < 0.4 ? { EMBEDDING_NEXT_MODEL: pick([B, C]), EMBEDDING_NEXT_BASE_URL: pick(['', SVC]) } : {}), ...(rand() < 0.3 ? { EMBEDDING_BASE_URL: SVC } : {}) };
  const steps = [];
  for (let s = 0; s < 9; s++) steps.push(rand() < 0.12 ? (rand() < 0.5 ? GET : { world: randomWorld() }) : post(randomBody()));
  add(`random-${n}`, { env, world: randomWorld(), steps });
}

// ── Running a scenario ──────────────────────────────────────────────────────────────────────────
const USERS = { admin: { id: 'qa-admin-1', role: 'admin' }, member: { id: 'qa-member-1', role: 'member' }, anon: null };
const read = (file) => { try { return fs.readFileSync(file, 'utf8'); } catch { return null; } };
async function run(sc) {
  const dir = path.join('qa', sc.name);
  const stateFile = path.join(dir, 'embedding-config.json');
  fs.mkdirSync(dir, { recursive: true });
  if (sc.file !== undefined) fs.writeFileSync(stateFile, sc.file);
  world = { ...defaults(), ...clone(sc.world) };
  let routes, config;
  const boot = () => {
    const warnings = [];
    const text = read(stateFile);
    let jsonError;
    if (text !== null) { try { JSON.parse(text); } catch (e) { jsonError = e.message; } }
    config = loadEmbeddingConfig({ env: sc.env, stateFile, nativeOwner, log: (line) => warnings.push(line) });
    const migration = createEmbeddingMigration({ config, rag, nativeOwner, env: sc.env, log: () => {} });
    routes = createEmbeddingRoutes({ json, readJson, migration, authService, fetchJson, DIARY_BASE: world.diaryBase, checkAddress, servedModels: (t) => rag.servedModels(t), service, resolveFile });
    return { warnings, ...(jsonError === undefined ? {} : { jsonError }) };
  };
  const out = { name: sc.name, env: sc.env, stateFile, file: sc.file ?? null, world: clone(world), boot: boot(), steps: [] };
  for (const step of sc.steps) {
    calls = [];
    const rec = { step: clone(step) };
    if (step.world) Object.assign(world, clone(step.world));
    else if (step.restart) rec.boot = boot();
    else if (step.writeFile !== undefined) { if (step.writeFile === null) fs.rmSync(stateFile, { force: true }); else fs.writeFileSync(stateFile, step.writeFile); }
    else if (step.refuse) { config.refuseSeeded(step.refuse, step.transient ? { transient: true } : undefined); rec.state = stateOf(config); }
    else if (step.release) { config.releaseSeeded(step.release); rec.state = stateOf(config); }
    else {
      const method = step.method || (step.get ? 'GET' : 'POST');
      const res = { setHeader() {} };
      let handled;
      try {
        handled = await routes({ method, raw: step.post }, res, { path: '/api/embedding', authn: USERS[step.as || 'admin'] ? { user: USERS[step.as || 'admin'] } : {} });
      } catch (e) {
        // index.cjs's request handler: an error the route throws is answered by errorResponse.
        const failure = errorResponse(e);
        json(res, failure.status, failure.body);
        handled = true;
      }
      if (!handled) throw Error(`${sc.name}: not handled`);
      rec.status = res.answer.status;
      rec.body = res.answer.body;
    }
    rec.calls = calls;
    rec.file = read(stateFile);
    out.steps.push(rec);
  }
  return out;
}
function json(res, status, body) { res.answer = { status, body: clone(body) }; }
// The configuration after a start-up refusal or release (config.get() without its change counter).
function stateOf(config) { const c = config.get(); return { active: c.active, next: c.next, previous: c.previous, source: c.source, damaged: config.damaged ?? null }; }
async function readJson(r) { return JSON.parse(r.raw ?? ''); }

// ── Direct cases: normalizeTarget, checkServed, seedFromEnv ─────────────────────────────────────
const NORMALIZE = [null, 5, 'qa', true, false, 0, [], {}, { model: A }, { model: ` ${A} `, profile: ` ${GEMMA} `, baseUrl: ` ${SVC}/ ` }, { model: A, profile: '' }, { model: A, profile: null },
  { model: A, profile: 'Nomic-v1' }, { model: A, profile: 'toString' }, { model: [A, B] }, { model: [[1, 2], null, 3] }, { model: {} }, { model: true }, { model: 1e21 }, { model: 0.000001 },
  ...odd.map((model) => ({ model })), ...urls.map((baseUrl) => ({ model: A, baseUrl })), { model: A, baseUrl: null }, { model: A, baseUrl: '   ' }, { model: A, baseUrl: false }, { model: A, baseUrl: [SVC] }];
const SERVED = [[A, [A]], [A, [A.toUpperCase()]], [A, [`${A}:latest`]], [`${A}:latest`, [A]], [`${A}:LATEST`, [` ${A} `]], [' ', ['']], ['', []], [A, []], [A, null], [A, ['/m/a.gguf']], [A, ['./a', '../b', '~/c']],
  [A, ['x.BIN', 'D:/qa', 'e:\\qa']], [A, ['x.safetensors', 'qa-b']], [A, ['x.ggufs']], ['\u03a3A\u03a3', ['\u03c3a\u03c2']], ['\u0130', ['i\u0307']], [ASTRAL, [ASTRAL.slice(0, 121), 'q'.repeat(130)]],
  [A, Array.from({ length: 30 }, (_, i) => `qa-${i}`)], ['qa-\ud83d', ['qa-\ud83d']]];
const SEEDS = [{}, { EMBEDDING_MODEL: A }, { EMBED_MODEL: ` ${A}\u00a0` }, { EMBEDDING_MODEL: '\ufeff', EMBED_MODEL: B }, { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: B }, { EMBEDDING_MODEL: A, EMBEDDING_NEXT_MODEL: A, EMBEDDING_NEXT_PROFILE: 'nomic-v1' },
  { EMBEDDING_MODEL: A, EMBEDDING_BASE_URL: SVC, EMBEDDING_NEXT_MODEL: A, EMBEDDING_NEXT_BASE_URL: ` ${SVC}` }, { EMBEDDING_MODEL: A, EMBEDDING_PROFILE: 'qa?', EMBEDDING_NEXT_MODEL: A, EMBEDDING_NEXT_PROFILE: 'qa?' },
  { EMBEDDING_NEXT_MODEL: 'default' }, { EMBEDDING_NEXT_MODEL: ' qa x ', EMBEDDING_NEXT_PROFILE: '  ' }, { EMBEDDING_BASE_URL: 'http://qa.invalid/', EMBEDDING_NEXT_MODEL: 'default', EMBEDDING_NEXT_BASE_URL: 'http://qa.invalid' }];
const err = (e) => ({ message: e.message, status: e.status ?? null, code: e.code ?? null });

(async () => {
  const work = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-embed-record-'));
  process.chdir(work);
  try {
    const normalize = NORMALIZE.map((input) => { try { return { input, ok: normalizeTarget(input) }; } catch (e) { return { input, error: err(e) }; } });
    const served = [];
    for (const [model, ids] of SERVED) served.push({ model, ids, verdict: await checkServed(async () => { if (!ids) throw Error('qa'); return ids.slice(); }, { model }) });
    const seeds = SEEDS.map((env) => { const warnings = []; return { env, state: seedFromEnv(env, 'default', (l) => warnings.push(l)), warnings }; });
    const recorded = [];
    for (const sc of scenarios) recorded.push(await run(sc));
    const fixture = { version: 1, sources: SOURCES, node: process.version, normalize, served, seeds, scenarios: recorded };
    process.stdout.write(`${JSON.stringify(fixture, null, 1)}\n`);
  } finally {
    process.chdir(os.tmpdir());
    fs.rmSync(work, { recursive: true, force: true });
  }
})().catch((e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
