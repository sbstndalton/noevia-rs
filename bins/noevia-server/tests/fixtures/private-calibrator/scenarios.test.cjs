'use strict';
// R6 C1: scripted scenarios for the auto-tune's PRIVATE calibrator, for the branches core's own
// llamacpp-full-autotune tests barely reach (foreign clients, cancels, failed restores, memory, engine
// errors). It builds a native manager over a synthetic engine (QA-named models, temp dir), takes the
// manager's own contextFactory and drives it as the runner does. Recorded by record-node.cjs with
// calibrator-preload.cjs (traces S<NNN>-*.json); whatever Node does is the oracle, so the tests only
// check that each scenario reached the branch it is about.
// Env: TRACE_CORE (core checkout; default ~/noevia-work/wt/track-R6-noevia-core).
const nodeTest = require('node:test'), assert = require('node:assert/strict');
// Every scenario has its own deadline: a scenario that never settles fails alone, the rest still run.
const SCENARIO_TIMEOUT_MS = 15000;
const test = (name, fn) => nodeTest(name, { timeout: SCENARIO_TIMEOUT_MS }, fn);
const fs = require('node:fs'), os = require('node:os'), path = require('node:path');

const CORE = path.resolve(process.env.TRACE_CORE || path.join(os.homedir(), 'noevia-work/wt/track-R6-noevia-core'));
const SERVER = path.join(CORE, 'server');
const { createModelManager } = require(path.join(SERVER, 'model-manager.cjs'));
const full = require(path.join(SERVER, 'llamacpp-full-autotune.cjs'));

const turn = () => new Promise(r => setImmediate(r));
const sse = obj => `data: ${JSON.stringify(obj)}\n\n`;
const markerOf = text => /start marker: (CAL-[\d-]+)/.exec(text)[1];

// A native manager over a synthetic engine. `engine` hooks: onModels(rows, state) before a listing,
// load(model, options, state) → reply or undefined, stream(body, options, signal, state) → Response or
// undefined, request(path, body, opts, state) → reply or undefined (any other path).
function fixture(t, { engine = {}, calibration = {}, models = ['qa-model'] } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-private-cal-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const ini = path.join(dir, 'models.ini');
  fs.writeFileSync(ini, 'version = 1\n' + [...models, 'qa-other', 'embed'].map(m => `[${m}]\nmodel = /models/${m}.gguf\nctx-size = 8192\nparallel = 1\n`).join(''));
  const state = { status: Object.fromEntries([...models, 'qa-other', 'embed'].map(m => [m, 'unloaded'])), extra: {}, loads: 0, listings: 0 };
  let manager;
  const options = m => manager.presets?.get?.(m)?.options || {};
  const fetchJson = async (url, opts = {}) => {
    const u = new URL(url), b = opts.body ? JSON.parse(opts.body) : {};
    if (opts.signal?.aborted) throw Error('aborted');
    if (u.pathname === '/models' && u.searchParams.has('reload')) return { ok: true, body: {} };
    if (u.pathname === '/models') {
      state.listings++;
      const rows = Object.entries(state.status).map(([id, value]) => ({ id, status: { value, args: id === 'embed' ? ['--embedding'] : [], ...(state.extra[id] || {}) }, meta: { n_ctx_train: 16384 } }));
      const changed = await engine.onModels?.(rows, state);
      return { ok: true, body: { data: changed || rows } };
    }
    if (u.pathname === '/models/load') {
      state.loads++;
      const replaced = await engine.load?.(b.model, state);
      if (replaced) return replaced;
      state.status[b.model] = 'loaded';
      return { ok: true, body: {} };
    }
    if (u.pathname === '/models/unload') { state.status[b.model] = 'unloaded'; return { ok: true, body: {} }; }
    const replaced = await engine.request?.(u.pathname, b, opts, state);
    if (replaced) return replaced;
    if (u.pathname === '/tokenize') return { ok: true, body: { tokens: Array(600).fill(1) } };
    if (u.pathname === '/props') return { ok: true, body: { build_info: 'qa-build' } };
    if (u.pathname === '/v1/chat/completions') return { ok: true, body: { choices: [{ message: { content: 'OK' } }] } };
    return { ok: true, body: {} };
  };
  const fetchStream = async (url, opts) => {
    const b = JSON.parse(opts.body), text = b.messages[0].content;
    const replaced = await engine.stream?.(b, opts.signal, state);
    if (replaced) return replaced;
    return new Response(sse({ choices: [{ delta: { content: markerOf(text) } }], timings: { prompt_n: Math.floor(text.length / 5), prompt_ms: 100, prompt_per_second: 100000 } }) + 'data: [DONE]\n\n');
  };
  let deps = null;
  const original = full.createFullAutotuner;
  full.createFullAutotuner = d => { deps = d; return original(d); };
  try {
    manager = createModelManager({ kind: 'llamacpp', baseUrl: 'http://synthetic', presetPath: ini, fetchJson, fetchStream,
      calibrationStatePath: path.join(dir, 'cal.json'), autotuneStatePath: path.join(dir, 'tune.json'),
      calibrationOptions: { identityFor: async () => ({ arch: null, quant: null, artifact: 'qa-artifact', build: null, profile: require('node:crypto').createHash('sha256').update(fs.readFileSync(ini)).digest('hex'), hardware: null }),
        sleep: async () => {}, readMemory: () => 20, ...calibration },
      autotuneOptions: { readMemory: () => 20, identityFor: async m => ({ arch: 'qa', quant: 'Q8_0', artifact: 'qa:' + m, build: 'qa', profile: '0'.repeat(64), hardware: 'qa' }) } });
  } finally { full.createFullAutotuner = original; }
  assert.ok(deps, 'the manager built an auto-tuner');
  // The recorder's factory when recording; the manager's own otherwise (baseline run).
  const factory = globalThis.__privateCalibratorFactory ? globalThis.__privateCalibratorFactory(deps) : deps.contextFactory;
  const hold = async () => {
    const release = await deps.maintenance.holdWhenIdle('QA private calibrator scenario', { timeoutMs: 5000 });
    t.after(() => { try { release(); } catch {} });
    return release;
  };
  const write = body => deps.applyUnlocked(body, {});
  const fillHooks = (extra = {}) => ({ applyUnlocked: write, onWrite: () => {}, onUpdate: () => {}, ...extra });
  const contextHooks = (extra = {}) => ({
    contextCurrent: () => true, applyUnlocked: write,
    foreignHeld: rows => rows.filter(m => m.id !== 'qa-model' && ['loaded', 'loading', 'sleeping'].includes(m.status?.value)).map(m => m.id),
    foreignWait: async () => {}, onWrite: () => {}, onUpdate: () => {}, ...extra });
  const revision = () => deps.presets.snapshot().revision;
  const probeOpts = (o = {}) => ({ ctx: 8192, fill: 4096, base: { 'cache-type-k': 'bf16', 'cache-type-v': 'bf16' }, baseRevision: revision(), promptBudgetSeconds: 120, evidence: true, ...o });
  return { ini, state, deps, factory, hold, fillHooks, contextHooks, probeOpts, revision };
}

// The runner's context stage: start, completion, status.
async function stage(f, hooks, { abortAfterStart = false } = {}) {
  const ctx = f.factory(hooks);
  const started = await ctx.start('qa-model', { confirmPause: true, promptBudgetSeconds: 120 });
  if (abortAfterStart) ctx.compareAndAbort(ctx.cancellationFacts().token);
  await ctx.completion();
  return { ctx, started, job: ctx.status('qa-model').body.job };
}

// A streamed body that sends one progress event, then waits until `signal` aborts (then errors).
function hangingStream(signal, onOpen) {
  return new Response(new ReadableStream({
    start(controller) {
      controller.enqueue(new TextEncoder().encode(sse({ prompt_progress: { total: 10000, processed: 100, cache: 0, time_ms: 10 } })));
      const fail = () => controller.error(Object.assign(Error('This operation was aborted'), { name: 'AbortError' }));
      if (signal?.aborted) fail(); else signal?.addEventListener('abort', fail, { once: true });
      onOpen?.();
    },
  }));
}

// ── 1. probe failures with evidence ────────────────────────────────────────────────────────────
test('probe: a long prompt rejected with HTTP 500 keeps the engine body as evidence', async t => {
  const f = fixture(t, { engine: { stream: () => new Response('{"error":{"message":"qa engine refused the prompt"}}', { status: 500 }) } });
  await f.hold();
  const r = await f.factory(f.fillHooks()).probe('qa-model', f.probeOpts());
  assert.equal(r.passed, false); assert.equal(r.evidence?.status, 500);
});

test('probe: an SSE error event fails the step with the engine words only as evidence', async t => {
  const f = fixture(t, { engine: { stream: () => new Response(sse({ error: { code: 507, message: 'qa out of memory' } })) } });
  await f.hold();
  const r = await f.factory(f.fillHooks()).probe('qa-model', f.probeOpts());
  assert.equal(r.passed, false); assert.equal(r.cause, 'oom');
});

test('probe: a refused load (409 with a body) is a load failure with evidence', async t => {
  const f = fixture(t, { engine: { load: () => ({ ok: false, status: 409, body: { error: 'qa budget refused this load' } }) } });
  await f.hold();
  const r = await f.factory(f.fillHooks()).probe('qa-model', f.probeOpts());
  assert.equal(r.passed, false); assert.equal(r.cause, 'load');
});

test('probe: a row that failed with an exit code is a load failure', async t => {
  const f = fixture(t, { engine: { load: (m, s) => { s.status[m] = 'unloaded'; s.extra[m] = { failed: true, exit_code: 137 }; return { ok: true, body: {} }; } } });
  await f.hold();
  const r = await f.factory(f.fillHooks()).probe('qa-model', f.probeOpts());
  assert.equal(r.passed, false); assert.equal(r.evidence?.exitCode, 137);
});

test('probe: a fast long prompt that recalls the marker passes', async t => {
  const f = fixture(t);
  await f.hold();
  const r = await f.factory(f.fillHooks()).probe('qa-model', f.probeOpts({ evidence: false, base: { parallel: '2' } }));
  assert.equal(r.passed, true);
});

test('probe: a slow prompt forecast over the budget fails on time', async t => {
  const f = fixture(t, { engine: { stream: () => new Response(sse({ prompt_progress: { total: 100000, processed: 1000, cache: 0, time_ms: 5000 } }) + sse({ choices: [{ delta: { content: 'x' } }] }) + 'data: [DONE]\n\n') } });
  await f.hold();
  const r = await f.factory(f.fillHooks()).probe('qa-model', f.probeOpts());
  assert.equal(r.passed, false); assert.equal(r.cause, 'time');
});

test('probe: another client loading a model while the probe loads is fatal (no foreign wait)', async t => {
  const f = fixture(t, { engine: { load: (m, s) => { s.status[m] = 'loading'; s.status['qa-other'] = 'loaded'; return { ok: true, body: {} }; } } });
  await f.hold();
  await assert.rejects(f.factory(f.fillHooks()).probe('qa-model', f.probeOpts()), /Another client loaded a model/);
});

// ── 2. probe cancels ───────────────────────────────────────────────────────────────────────────
test('probe: cancelProbe while the long prompt streams stops it as cancelled', async t => {
  let ctx;
  const f = fixture(t, { engine: { stream: (b, signal) => hangingStream(signal, () => setImmediate(() => ctx.cancelProbe())) } });
  await f.hold();
  ctx = f.factory(f.fillHooks());
  await assert.rejects(ctx.probe('qa-model', f.probeOpts()), e => e.cancelled === true);
});

test('probe: cancelProbe from inside the load poll stops it as cancelled', async t => {
  let ctx, cancel = false;
  const f = fixture(t, { engine: {
    load: (m, s) => { s.status[m] = 'loading'; cancel = true; return { ok: true, body: {} }; },
    onModels: () => { if (cancel) { cancel = false; ctx.cancelProbe(); } },
  } });
  await f.hold();
  ctx = f.factory(f.fillHooks());
  await assert.rejects(ctx.probe('qa-model', f.probeOpts()), e => e.cancelled === true);
});

// ── 3. foreign clients in a context stage ──────────────────────────────────────────────────────
test('context: another client loading during the load poll is waited for and the step repeated', async t => {
  let once = true;
  const f = fixture(t, { engine: { load: (m, s) => { s.status[m] = 'loaded'; if (once) { once = false; s.status['qa-other'] = 'loaded'; } return { ok: true, body: {} }; } } });
  await f.hold();
  const waits = [];
  const { job } = await stage(f, f.contextHooks({ foreignWait: async e => { waits.push(e.message); f.state.status['qa-other'] = 'unloaded'; } }));
  assert.ok(waits.length >= 1); assert.equal(job.status, 'passed', job.error);
});

test('context: a foreign wait that gives up stops the run', async t => {
  const f = fixture(t, { engine: { load: (m, s) => { s.status[m] = 'loaded'; s.status['qa-other'] = 'loaded'; return { ok: true, body: {} }; } } });
  await f.hold();
  const { job } = await stage(f, f.contextHooks({ foreignWait: async () => { throw Object.assign(Error('Another client kept using qa-other for 15 min.'), { fatal: true, foreignTimeout: true }); } }));
  assert.equal(job.status, 'failed');
});

test('context: a sleeping model foreignHeld does not claim is unloaded before the run', async t => {
  const f = fixture(t);
  f.state.status['qa-other'] = 'sleeping';
  await f.hold();
  const { job } = await stage(f, f.contextHooks({ foreignHeld: () => [] }));
  assert.equal(job.status, 'passed', job.error);
});

test('context: a model foreignHeld claims makes unloadAll wait for it', async t => {
  const f = fixture(t);
  f.state.status['qa-other'] = 'loaded';
  await f.hold();
  let waited = 0;
  const { job } = await stage(f, f.contextHooks({ foreignWait: async () => { waited++; f.state.status['qa-other'] = 'unloaded'; } }));
  assert.equal(waited, 1); assert.equal(job.status, 'passed', job.error);
});

test('context: a failed smoke request while another client is live is waited for and repeated', async t => {
  let once = true;
  const f = fixture(t, { engine: { request: (p, b, o, s) => {
    if (p === '/v1/chat/completions' && once) { once = false; s.status['qa-other'] = 'loaded'; return { ok: false, status: 503, body: {} }; }
  } } });
  await f.hold();
  const { job } = await stage(f, f.contextHooks({ foreignWait: async () => { f.state.status['qa-other'] = 'unloaded'; } }));
  assert.equal(job.status, 'passed', job.error);
});

// ── 4. aborts ──────────────────────────────────────────────────────────────────────────────────
test('context: an abort during the long prompt cancels and restores the profile', async t => {
  let ctx;
  const f = fixture(t, { engine: { stream: (b, signal) => hangingStream(signal, () => setImmediate(() => ctx.compareAndAbort(ctx.cancellationFacts().token))) } });
  await f.hold();
  ctx = f.factory(f.contextHooks());
  const started = await ctx.start('qa-model', { confirmPause: true, promptBudgetSeconds: 120 });
  assert.equal(started.ok, true);
  await ctx.completion();
  const job = ctx.status('qa-model').body.job;
  assert.equal(job.status, 'cancelled'); assert.equal(job.restored, true);
});

test('context: an abort while start reads the identity answers stale', async t => {
  let ctx, reads = 0;
  const f = fixture(t, { calibration: { identityFor: async () => {
    if (++reads === 2) ctx.compareAndAbort(ctx.cancellationFacts().token);
    return { arch: null, quant: null, artifact: 'qa-artifact', build: null, profile: 'a'.repeat(64), hardware: null };
  } } });
  await f.hold();
  ctx = f.factory(f.contextHooks());
  const started = await ctx.start('qa-model', { confirmPause: true, promptBudgetSeconds: 120 });
  assert.equal(started.ok, false);
  for (let i = 0; i < 10; i++) await turn();
});

test('context: a stage that is no longer current answers stale at once', async t => {
  const f = fixture(t);
  await f.hold();
  const { started } = await stage(f, f.contextHooks({ contextCurrent: () => false }));
  assert.equal(started.ok, false);
});

test('context: a stage whose current() turns false during the reads answers stale', async t => {
  const f = fixture(t);
  await f.hold();
  let n = 0;
  const { started } = await stage(f, f.contextHooks({ contextCurrent: () => ++n < 3 }));
  assert.equal(started.ok, false);
});

test('context: a start whose identity is invalid answers stale', async t => {
  const f = fixture(t, { calibration: { identityFor: async () => ({ arch: null }) } });
  await f.hold();
  const { started } = await stage(f, f.contextHooks());
  assert.equal(started.ok, false);
});

test('context: an out-of-range prompt budget is refused as an invalid launch', async t => {
  const f = fixture(t);
  await f.hold();
  const ctx = f.factory(f.contextHooks());
  await assert.rejects(ctx.start('qa-model', { confirmPause: true, promptBudgetSeconds: 5 }), /invalid prepared launch/);
});

// ── 5. failed runs and restores ────────────────────────────────────────────────────────────────
test('context: a refused final write fails the run and restores the profile', async t => {
  const f = fixture(t);
  await f.hold();
  // The final write is the one made once the search has chosen a size (job.result.verifiedCtx set;
  // onUpdate hands over the live job object). Every earlier write is a step's test profile.
  let live = null, refused = 0;
  const hooks = f.contextHooks({ onUpdate: job => { if (job) live = job; } });
  const real = hooks.applyUnlocked;
  hooks.applyUnlocked = async body => {
    if (live?.result?.verifiedCtx == null) return real(body);
    refused++;
    return { ok: false, status: 409, body: { error: 'QA refused the final write.' } };
  };
  const { job } = await stage(f, hooks);
  assert.equal(refused, 1);
  assert.equal(job.status, 'failed'); assert.match(job.error, /QA refused the final write/); assert.equal(job.restored, true);
});

test('context: a calibrated profile that does not load fails the run', async t => {
  let loads = 0, total = 0;
  const f = fixture(t, { engine: { load: (m, s) => { total++; if (s.final) { loads++; s.status[m] = 'unloaded'; return { ok: true, body: {} }; } } } });
  await f.hold();
  const hooks = f.contextHooks({ onUpdate: job => { if (job?.phase === 'Loading the calibrated profile') f.state.final = true; } });
  const { job } = await stage(f, hooks);
  assert.equal(job.status, 'failed'); assert.ok(loads >= 1 && total > loads);
});

test('context: an unacknowledged baseline invalidation restores without a reload', async t => {
  let ctx;
  const f = fixture(t, { calibration: { invalidateBaseline: async () => false },
    engine: { stream: (b, signal) => hangingStream(signal, () => setImmediate(() => ctx.compareAndAbort(ctx.cancellationFacts().token))) } });
  await f.hold();
  ctx = f.factory(f.contextHooks());
  await ctx.start('qa-model', { confirmPause: true, promptBudgetSeconds: 120 });
  await ctx.completion();
  const job = ctx.status('qa-model').body.job;
  assert.match(job.error, /Reload failed or timed out/);
});

test('context: models.ini changed outside the run is not restored', async t => {
  let ctx;
  const f = fixture(t, { engine: { stream: (b, signal) => hangingStream(signal, () => setImmediate(() => {
    fs.appendFileSync(f.ini, '[qa-added]\nmodel = /models/qa-added.gguf\n');
    ctx.compareAndAbort(ctx.cancellationFacts().token);
  })) } });
  await f.hold();
  ctx = f.factory(f.contextHooks());
  await ctx.start('qa-model', { confirmPause: true, promptBudgetSeconds: 120 });
  await ctx.completion();
  const job = ctx.status('qa-model').body.job;
  assert.equal(job.restored, false);
});

test('context: no size passes the long prompt and the run fails', async t => {
  const f = fixture(t, { engine: { stream: () => new Response(sse({ choices: [{ delta: { content: 'forgot' } }], timings: { prompt_n: 10, prompt_ms: 100 } }) + 'data: [DONE]\n\n') } });
  await f.hold();
  const { job } = await stage(f, f.contextHooks());
  assert.equal(job.status, 'failed');
});

test('context: a model that does not load at the starting size fails the run', async t => {
  const f = fixture(t, { engine: { load: (m, s) => { s.status[m] = 'unloaded'; return { ok: true, body: {} }; } } });
  await f.hold();
  const { job } = await stage(f, f.contextHooks());
  assert.equal(job.status, 'failed');
});

// ── 6. memory ──────────────────────────────────────────────────────────────────────────────────
// A probe's step reads memory twice before it starts (the wait), once for the guard and once for the
// first sample, then once per load poll (5th read) and once when the row reads loaded (6th read).
test('memory: available memory under the floor once the model reads loaded fails that size as oom', async t => {
  let reads = 0;
  const f = fixture(t, { calibration: { timeouts: { memoryPoll: 1e9 }, readMemory: () => (++reads === 6 ? 1 : 20) } });
  await f.hold();
  const r = await f.factory(f.fillHooks()).probe('qa-model', f.probeOpts());
  assert.equal(r.passed, false); assert.equal(r.cause, 'oom');
});

test('memory: available memory under the floor at a load poll fails that size as oom', async t => {
  let reads = 0;
  const f = fixture(t, { calibration: { timeouts: { memoryPoll: 1e9 }, readMemory: () => (++reads === 5 ? 1 : 20) } });
  await f.hold();
  const r = await f.factory(f.fillHooks()).probe('qa-model', f.probeOpts());
  assert.equal(r.passed, false); assert.equal(r.cause, 'oom');
});

test('memory: memory that recovers while a step waits for it lets the step run', async t => {
  let reads = 0;
  const f = fixture(t, { calibration: { timeouts: { memoryPoll: 1e9 }, readMemory: () => (++reads <= 4 ? 1 : 20) } });
  await f.hold();
  const r = await f.factory(f.fillHooks()).probe('qa-model', f.probeOpts({ evidence: false }));
  assert.equal(r.passed, true);
});

test('memory: memory that stays under the floor before a step is fatal', async t => {
  const f = fixture(t, { calibration: { timeouts: { memoryPoll: 1e9 }, readMemory: () => 1 } });
  await f.hold();
  await assert.rejects(f.factory(f.fillHooks()).probe('qa-model', f.probeOpts()), /Available memory stayed below/);
});

test('memory: an unreadable memory value marks the guard unavailable', async t => {
  const f = fixture(t, { calibration: { timeouts: { memoryPoll: 1e9 }, readMemory: () => null } });
  await f.hold();
  const { job } = await stage(f, f.contextHooks());
  assert.equal(job.memoryGuard, 'unavailable');
});

// ── 7. engine failures ─────────────────────────────────────────────────────────────────────────
test('engine: a request that throws mid-step reads as a crash once the server answers again', async t => {
  let once = true;
  const f = fixture(t, { engine: { request: p => { if (p === '/tokenize' && once) { once = false; } if (p === '/v1/chat/completions' && once === false) { once = null; throw Error('qa socket hang up'); } } } });
  await f.hold();
  const { job } = await stage(f, f.contextHooks());
  assert.ok(['passed', 'failed'].includes(job.status));
});

test('engine: models that never unload fail the run', async t => {
  const f = fixture(t, { calibration: { timeouts: { unload: 30, poll: 10 } } });
  f.state.status['qa-other'] = 'loaded';
  const realUnload = f.state;
  await f.hold();
  // qa-other ignores unload requests. Each "did not unload" is handed to foreignWait (qa-other is
  // live), which gives up on the third one, as the runner's wait does once its deadline passes.
  const waits = [];
  const hooks = f.contextHooks({ foreignHeld: () => [], foreignWait: async e => {
    waits.push(e.message);
    if (waits.length >= 3) throw Object.assign(Error('Another client kept using qa-other for 15 min.'), { fatal: true, foreignTimeout: true });
  } });
  const { job } = await (async () => {
    const ctx = f.factory(hooks);
    const orig = Object.getOwnPropertyDescriptor(realUnload.status, 'qa-other');
    Object.defineProperty(realUnload.status, 'qa-other', { get: () => 'loaded', set: () => {}, enumerable: true, configurable: true });
    t.after(() => Object.defineProperty(realUnload.status, 'qa-other', orig));
    await ctx.start('qa-model', { confirmPause: true, promptBudgetSeconds: 120 });
    await ctx.completion();
    return { job: ctx.status('qa-model').body.job };
  })();
  assert.equal(job.status, 'failed');
  assert.deepEqual(waits, Array(3).fill('Models did not unload in time.'));
});

test('engine: a listing that stops answering after a crash fails the run as the server gone', async t => {
  let down = false;
  // Short deadlines: without the recorder's clock the calibrator's own time is real, and its sleeps
  // here return at once.
  const f = fixture(t, { calibration: { timeouts: { recover: 100, unload: 50, poll: 10 } }, engine: {
    request: p => { if (p === '/v1/chat/completions' && !down) { down = true; throw Error('qa connection reset'); } },
    onModels: () => { if (down) throw Error('qa router down'); },
  } });
  await f.hold();
  const { job } = await stage(f, f.contextHooks());
  assert.equal(job.status, 'failed'); assert.match(job.error, /did not come back/);
});
