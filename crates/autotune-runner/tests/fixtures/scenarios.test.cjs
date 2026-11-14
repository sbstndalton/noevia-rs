'use strict';
// R6 shift 3: scripted scenarios for the auto-tune RUNNER's cancellation primitive (#2294
// `compareAndAbort`, llamacpp-full-autotune.cjs createAutotuneCancellation) for what core's own
// llamacpp-full-autotune tests never record: an abort whose token is no longer current (the front's
// cancellationFacts -> compareAndAbort race), aborts with no job or a malformed token, and the child
// aborts the runner makes (the bound calibrator abort of a context stage, the abort again once a
// stage's start answers, a planned fill probe's cancel). It builds core's native model manager over
// a synthetic engine (QA-named models, a temp models.ini) as core's tests do, and drives the manager's
// actor. Recorded by record-node.cjs with trace-preload.cjs (traces S<NNN>-*.json); whatever Node does
// is the oracle, so each scenario only checks that it reached the branch it is about.
// Env: TRACE_CORE (core checkout, required), DAV_PARSE_WASM (the pinned module when it is not in
// <core>/server/wasm).
const nodeTest = require('node:test'), assert = require('node:assert/strict');
// Every scenario has its own deadline: a scenario that never settles fails alone, the rest still run.
const SCENARIO_TIMEOUT_MS = 60000;
const test = (name, fn) => nodeTest(name, { timeout: SCENARIO_TIMEOUT_MS }, fn);
const fs = require('node:fs'), os = require('node:os'), path = require('node:path'), crypto = require('node:crypto');

if (!process.env.TRACE_CORE) throw Error('scenarios.test.cjs: TRACE_CORE (the core checkout) is required');
const SERVER = path.join(path.resolve(process.env.TRACE_CORE), 'server');
const { createModelManager } = require(path.join(SERVER, 'model-manager.cjs'));
const { createPresetStore } = require(path.join(SERVER, 'llamacpp-presets.cjs'));
const full = require(path.join(SERVER, 'llamacpp-full-autotune.cjs'));
const { QUALITY } = full;

const identity = m => ({ arch: 'qa', quant: 'Q8_0', artifact: 'qa:' + m, build: 'qa-v1', profile: '0'.repeat(64), hardware: 'qa' });
const unq = o => ['bf16', 'f16'].includes(o['cache-type-k']);
const quantized = o => ['q5_0', 'q5_1', 'q4_0'].includes(o['cache-type-k']);
// The planned order (#1003), as core's tests set it up: the wasm planner over fixed model facts.
const QA_FACTS = { meta: { contextLength: 16384, blockCount: 16, headCount: 16, headCountKv: 4, embeddingLength: 2048 }, modelBytes: 1e9, mmprojBytes: 0 };
const planned = () => {
  const w = require(path.join(SERVER, 'dav-parse-wasm.cjs'));
  return { planner: { plan: r => w.autotunePlan(r) }, planFacts: async () => QA_FACTS, budgetGib: () => 16,
    servingChecks: { templateCaps: () => { throw Error('unusable'); }, verdict: () => { throw Error('unusable'); } } };
};

// A native manager over a synthetic engine that tunes `qa-model` to a pass (core's fixture defaults:
// quantized KV answers wrong, bf16/f16 recall up to 8192, the deep MTP draft is fastest).
// Hooks: onChat({ manager, chats, o, prompt }) inside a chat request, onStream({ manager, o, signal })
// inside a long prompt; wrapAutotuner(deps) -> deps for the manager's runner.
function fixture(t, { onChat = null, onStream = null, autotuneExtra = {}, wrapAutotuner = null } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-runner-cancel-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const ini = path.join(dir, 'models.ini'), models = ['qa-model', 'embed'];
  fs.writeFileSync(ini, 'version = 1\n' + models.map(m => `[${m}]\nmodel = /models/${m}.gguf\nctx-size = 8192\nparallel = 1\n`).join(''));
  const presets = createPresetStore(ini), options = m => presets.get(m).options;
  const status = Object.fromEntries(models.map(m => [m, 'unloaded']));
  let manager = null, chats = 0;
  const fetchJson = async (url, opts = {}) => {
    const u = new URL(url), b = opts.body ? JSON.parse(opts.body) : {};
    if (opts.signal?.aborted) throw Error('aborted');
    if (u.pathname === '/models' && u.searchParams.has('reload')) return { ok: true, body: {} };
    if (u.pathname === '/models') return { ok: true, body: { data: Object.entries(status).map(([id, value]) =>
      ({ id, status: { value, args: id === 'embed' ? ['--embedding'] : [] }, meta: { n_ctx_train: 16384 } })) } };
    if (u.pathname === '/models/load') { status[b.model] = 'loaded'; return { ok: true, body: {} }; }
    if (u.pathname === '/models/unload') { status[b.model] = 'unloaded'; return { ok: true, body: {} }; }
    if (u.pathname === '/tokenize') return { ok: true, body: { tokens: Array(600).fill(1) } };
    if (u.pathname === '/props') return { ok: true, body: { build_info: 'qa-v1' } };
    if (u.pathname === '/v1/chat/completions') {
      if (b.messages[0].role === 'system') return { ok: true, status: 200, body: { choices: [{ message: { content: 'Synthetic answer' } }] } };
      chats++;
      const o = options(b.model), prompt = b.messages[0].content;
      await onChat?.({ manager, chats, o, prompt });
      if (opts.signal?.aborted) throw Error('aborted');
      const q = QUALITY.find(x => x.prompt === prompt);
      let text = q ? q.expected : prompt.startsWith('List the whole numbers') ? Array.from({ length: 60 }, (_, i) => i + 1).join(', ') : 'Synthetic answer';
      if (q && quantized(o)) text = 'wrong';
      const spec = o['spec-type'], draft = spec === 'ngram-simple' || spec === 'draft-mtp';
      const speed = spec === 'draft-mtp' ? (o['spec-draft-n-max'] === '8' ? 50 : 36) : spec === 'ngram-simple' ? 26 : 20;
      return { ok: true, body: { choices: [{ message: { content: text } }], timings: {
        predicted_per_second: speed * (quantized(o) ? 2 : o['cache-type-k'] === 'q8_0' ? 1.2 : 1),
        prompt_per_second: o['ubatch-size'] === '1024' ? 800 : 500, draft_n: draft ? 100 : 0, draft_n_accepted: draft ? 65 : 0 } } };
    }
    return { ok: true, body: {} };
  };
  const fetchStream = async (url, opts) => {
    const b = JSON.parse(opts.body), text = b.messages[0].content, marker = /start marker: (CAL-[\d-]+)/.exec(text)[1];
    const o = options(b.model);
    await onStream?.({ manager, o, signal: opts.signal });
    if (opts.signal?.aborted) throw Object.assign(Error('This operation was aborted'), { name: 'AbortError' });
    const content = Number(o['ctx-size']) <= (unq(o) ? 8192 : 16384) ? marker : 'forgot';
    return new Response(`data: ${JSON.stringify({ choices: [{ delta: { content } }], timings: { prompt_n: Math.floor(text.length / 5), prompt_ms: 100, prompt_per_second: 100000 } })}\n\ndata: [DONE]\n\n`);
  };
  const make = () => createModelManager({ kind: 'llamacpp', baseUrl: 'http://synthetic', presetPath: ini, fetchJson, fetchStream,
    calibrationStatePath: path.join(dir, 'cal.json'), autotuneStatePath: path.join(dir, 'tune.json'),
    calibrationOptions: { identityFor: async () => ({ arch: null, quant: null, artifact: 'qa-artifact', build: null,
      profile: crypto.createHash('sha256').update(fs.readFileSync(ini)).digest('hex'), hardware: null }), sleep: async () => {}, readMemory: () => 20 },
    autotuneOptions: { ...autotuneExtra, betweenModelsMs: 25, readMemory: () => 20, identityFor: async m => identity(m) } });
  if (wrapAutotuner) {
    const original = full.createFullAutotuner;
    full.createFullAutotuner = deps => original(wrapAutotuner(deps));
    try { manager = make(); } finally { full.createFullAutotuner = original; }
  } else manager = make();
  return { manager, ini };
}
async function finished(manager) {
  for (let i = 0; i < 20000; i++) {
    const j = manager.autotune.statusFacts().job;
    if (j && j.status !== 'running') return j;
    await new Promise(r => setImmediate(r));
  }
  throw Error('The QA tune did not finish');
}
const cancelNow = a => a.compareAndAbort(a.cancellationFacts().token);

// ── 1. the front's facts -> abort race (a token that is no longer current) ───────────────────────
test('race: a token read during one chat is stale at the next; the abort is refused and the tune passes', async t => {
  let facts = null, refused = null;
  const f = fixture(t, { onChat: ({ manager, chats }) => {
    if (chats === 2) facts = manager.autotune.cancellationFacts();
    if (chats === 3) refused = manager.autotune.compareAndAbort(facts.token);
  } });
  assert.equal((await f.manager.autotune.start('qa-model', { confirmPause: true })).status, 202);
  const j = await finished(f.manager);
  assert.deepEqual(refused, { applied: false }); assert.equal(j.status, 'passed', j.error);
});

test('race: after a refused stale abort, a fresh read cancels the tune', async t => {
  let facts = null, refused = null, applied = null;
  const f = fixture(t, { onChat: ({ manager, chats }) => {
    if (chats === 2) facts = manager.autotune.cancellationFacts();
    if (chats === 3) { refused = manager.autotune.compareAndAbort(facts.token); applied = cancelNow(manager.autotune); }
  } });
  assert.equal((await f.manager.autotune.start('qa-model', { confirmPause: true })).status, 202);
  const j = await finished(f.manager);
  assert.deepEqual(refused, { applied: false }); assert.equal(applied.applied, true); assert.equal(j.status, 'cancelled');
});

test('race: a token read while the tune ran is refused once it finished; the finished job\'s current token applies', async t => {
  let facts = null;
  const f = fixture(t, { onChat: ({ manager, chats }) => { if (chats === 2) facts = manager.autotune.cancellationFacts(); } });
  assert.equal((await f.manager.autotune.start('qa-model', { confirmPause: true })).status, 202);
  const j = await finished(f.manager);
  assert.equal(j.status, 'passed', j.error);
  assert.deepEqual(f.manager.autotune.compareAndAbort(facts.token), { applied: false });
  assert.equal(typeof cancelNow(f.manager.autotune).applied, 'boolean');
});

// ── 2. tokens that cannot apply ─────────────────────────────────────────────────────────────────
test('no job: the current token, a malformed token, a foreign hash and non-strings are all refused', async t => {
  const f = fixture(t);
  const a = f.manager.autotune;
  const answers = [cancelNow(a), a.compareAndAbort('qa-not-a-token'), a.compareAndAbort('0'.repeat(64)), a.compareAndAbort(null), a.compareAndAbort(42)];
  assert.ok(answers.every(r => r.applied === false), JSON.stringify(answers));
});

// ── 3. the child aborts the runner makes ────────────────────────────────────────────────────────
test('child: a cancel during a context stage aborts its calibrator with the token the runner reads then', async t => {
  let applied = null;
  const f = fixture(t, { onStream: ({ manager }) => { if (applied === null) applied = cancelNow(manager.autotune).applied; } });
  assert.equal((await f.manager.autotune.start('qa-model', { confirmPause: true })).status, 202);
  const j = await finished(f.manager);
  assert.equal(applied, true); assert.equal(j.status, 'cancelled');
});

test('child: a cancel just as a context stage\'s start answers aborts its calibrator again after start', async t => {
  // The stage's start answers ok and the cancel lands before the runner resumes (a cancel between
  // two turns): the runner aborts the calibrator in compareAndAbort, then again because `cancelled`
  // is set when start returns.
  let manager = null, armed = true, applied = null, aborts = 0;
  const f = fixture(t, { wrapAutotuner: deps => ({ ...deps, contextFactory: hooks => {
    const child = deps.contextFactory(hooks);
    if (typeof hooks?.contextCurrent !== 'function') return child;
    return { ...child,
      compareAndAbort: token => { aborts++; return child.compareAndAbort(token); },
      start: async (...args) => {
        const r = await child.start(...args);
        if (armed && r?.ok) { armed = false; applied = cancelNow(manager.autotune).applied; }
        return r;
      } };
  } }) });
  manager = f.manager;
  assert.equal((await manager.autotune.start('qa-model', { confirmPause: true })).status, 202);
  const j = await finished(manager);
  assert.equal(applied, true); assert.equal(aborts, 2); assert.equal(j.status, 'cancelled');
});

test('child: a cancel during a planned fill probe cancels the probe through its wrapper', async t => {
  let applied = null;
  const f = fixture(t, { autotuneExtra: planned(), onStream: ({ manager }) => { if (applied === null) applied = cancelNow(manager.autotune).applied; } });
  assert.equal((await f.manager.autotune.start('qa-model', { confirmPause: true })).status, 202);
  const j = await finished(f.manager);
  assert.equal(applied, true); assert.equal(j.status, 'cancelled');
});
