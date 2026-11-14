'use strict';
// R6 C1 oracle recorder (preload) for the auto-tune's PRIVATE calibrator. Loaded with
// `node --require` before noevia-core's server/llamacpp-full-autotune.test.cjs; records every
// calibrator the manager's contextFactory builds (stateFile undefined) at its dependency boundary,
// one trace per instance (format: FORMAT.md beside this file). Read-only on the core checkout.
// Env: TRACE_CORE (core checkout root), TRACE_OUT (output directory).
const fs = require('node:fs'), path = require('node:path'), crypto = require('node:crypto');
const Module = require('node:module');
const { AsyncLocalStorage } = require('node:async_hooks');

const CORE = path.resolve(process.env.TRACE_CORE || '');
const OUT = path.resolve(process.env.TRACE_OUT || '');
if (!process.env.TRACE_CORE || !process.env.TRACE_OUT) throw Error('calibrator-preload: TRACE_CORE and TRACE_OUT are required');
const SERVER = path.join(CORE, 'server');
const CALIBRATION = path.join(SERVER, 'llamacpp-calibration.cjs');
const RUNNER = path.join(SERVER, 'llamacpp-full-autotune.cjs');
const MANAGER = path.join(SERVER, 'llamacpp-manager.cjs');
const TEST = path.join(SERVER, 'llamacpp-full-autotune.test.cjs');
const SCENARIOS = path.join(__dirname, 'scenarios.test.cjs');
// TRACE_PREFIX: prepended to every trace file name (scenario traces are `S<NNN>-...`).
const PREFIX = process.env.TRACE_PREFIX || '';
const sha = file => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const SOURCES = { calibration: sha(CALIBRATION), manager: sha(MANAGER), runner: sha(RUNNER),
  test: sha(PREFIX ? SCENARIOS : TEST) };
const T0 = 1700000000000;
const als = new AsyncLocalStorage();
const real = { writeFileSync: fs.writeFileSync, randomUUID: crypto.randomUUID, randomInt: crypto.randomInt, setTimeout: globalThis.setTimeout };

function callerFrame(fn) {
  const prep = Error.prepareStackTrace, limit = Error.stackTraceLimit;
  try {
    Error.prepareStackTrace = (_, frames) => frames; Error.stackTraceLimit = 1;
    const holder = {}; Error.captureStackTrace(holder, fn);
    const frames = holder.stack;
    return Array.isArray(frames) && frames[0] ? { file: frames[0].getFileName(), name: frames[0].getFunctionName() } : null;
  } catch { return null; } finally { Error.prepareStackTrace = prep; Error.stackTraceLimit = limit; }
}
function callerFile(fn) { return callerFrame(fn)?.file ?? null; }

// ── tests: name, ordinal and end of each node:test test ─────────────────────────────────────────
let testOrdinal = 0, currentTest = null;
const traces = [];
function wrapTestFn(info, fn) {
  const wrapped = function (t, ...rest) {
    currentTest = info;
    if (t && typeof t.after === 'function') t.after(() => { for (const tr of info.traces) close(tr); });
    return fn.call(this, t, ...rest);
  };
  return fn.length >= 2 ? function (t, done) { return wrapped.call(this, t, done); } : wrapped;
}
function wrapTest(realTest) {
  const wrappedTest = function test(...args) {
    const at = args.findIndex(a => typeof a === 'function');
    const name = typeof args[0] === 'string' ? args[0] : (at >= 0 && args[at].name) || '<anonymous>';
    const info = { name, ordinal: ++testOrdinal, traces: [] };
    if (at >= 0) args[at] = wrapTestFn(info, args[at]);
    return realTest.apply(this, args);
  };
  for (const key of Reflect.ownKeys(realTest)) {
    if (['length', 'name', 'prototype', 'arguments', 'caller'].includes(key)) continue;
    try { Object.defineProperty(wrappedTest, key, Object.getOwnPropertyDescriptor(realTest, key)); } catch { /* fixed */ }
  }
  wrappedTest.test = wrappedTest;
  return wrappedTest;
}
let wrappedNodeTest = null;
const realLoad = Module._load;
Module._load = function (request) {
  const loaded = realLoad.apply(this, arguments);
  if (request === 'node:test' || request === 'test') return (wrappedNodeTest ||= wrapTest(loaded));
  return loaded;
};

// ── values ──────────────────────────────────────────────────────────────────────────────────────
const TOKEN_KEYS = new Set(['token', 'generation', 'rowToken', 'preparedToken']);
function clone(v) {
  if (v === undefined) return undefined;
  let text;
  try { text = JSON.stringify(v, (k, x) => (TOKEN_KEYS.has(k) && typeof x === 'string' ? '<token>' : x)); } catch { return '<unserializable>'; }
  return text === undefined ? undefined : JSON.parse(text);
}
function thrown(e) {
  const flags = {};
  if (e && typeof e === 'object') {
    for (const k of ['cancelled', 'fatal', 'coverageLost', 'foreignTimeout', 'backend']) if (e[k]) flags[k] = true;
    if (e.name && e.name !== 'Error') flags.name = String(e.name);
  }
  return { message: e && typeof e === 'object' ? String(e.message ?? '') : String(e), flags };
}
const parseBody = b => { if (b == null) return null; if (typeof b !== 'string') return clone(b); try { return JSON.parse(b); } catch { return b; } };

// ── traces ──────────────────────────────────────────────────────────────────────────────────────
function newTrace(kind, callbacks) {
  const info = currentTest || { name: '<outside a test>', ordinal: 0, traces: [] };
  const tr = { test: info.name, ordinal: info.ordinal, instance: info.traces.length, kind, callbacks,
    replayable: true, why: [], config: { memoryFloorGib: 2, readMemory: undefined, timeouts: {} },
    events: [], stack: [], clock: T0, ints: 0, uuids: 0, streams: 0, closed: false, late: false, openAtEnd: 0 };
  info.traces.push(tr); traces.push(tr);
  return tr;
}
function unreplayable(tr, why) { tr.replayable = false; if (!tr.why.includes(why)) tr.why.push(why); }
function close(tr) {
  if (tr.closed) return;
  tr.closed = true;
  for (const ev of tr.stack) { ev.open = true; tr.openAtEnd++; }
  tr.stack = [];
}
function place(tr, ev) {
  if (tr.closed) { tr.late = true; return false; }
  const top = tr.stack[tr.stack.length - 1];
  if (top) (top.during ||= []).push(ev); else tr.events.push(ev);
  return true;
}
// A synchronous event: placed, never in flight.
function syncEvent(tr, ev) { place(tr, ev); return ev; }
// An asynchronous call: placed, in flight until `settle`.
function begin(tr, ev) { if (place(tr, ev)) tr.stack.push(ev); return ev; }
function settle(tr, ev, outcome) {
  const i = tr.stack.lastIndexOf(ev);
  if (i < 0) return; // closed before it settled (ev.open), or never placed
  tr.stack.splice(i, 1);
  Object.assign(ev, outcome);
}
const recording = () => { const s = als.getStore(); return s && s.tr && !s.foreign ? s.tr : null; };
const foreign = (tr, fn) => als.run({ tr, foreign: true }, fn);

// An async dependency the calibrator calls: recorded, run outside the recording.
function asyncDep(tr, fn, describe, resultOf = clone) {
  return function (...args) {
    const rec = recording() === tr;
    const ev = rec ? begin(tr, describe(...args)) : null;
    let p;
    try { p = Promise.resolve(foreign(tr, () => fn.apply(this, args))); } catch (e) { p = Promise.reject(e); }
    if (!ev) return p;
    return p.then(r => { settle(tr, ev, { result: resultOf(r) }); return r; }, e => { settle(tr, ev, { throws: thrown(e) }); throw e; });
  };
}

// ── the stream body ─────────────────────────────────────────────────────────────────────────────
function wrapBody(tr, body, index) {
  let reader = null, iterator = null;
  const pull = async () => {
    if (typeof body.getReader === 'function') { reader ||= body.getReader(); return reader.read(); }
    iterator ||= body[Symbol.asyncIterator]();
    return iterator.next();
  };
  const read = async () => {
    const rec = recording() === tr;
    const ev = rec ? begin(tr, { dep: 'stream.read', stream: index }) : null;
    try {
      const r = await foreign(tr, pull);
      if (ev) settle(tr, ev, { result: r.done ? { done: true } : { text: Buffer.from(r.value).toString('utf8') } });
      return r;
    } catch (e) { if (ev) settle(tr, ev, { throws: thrown(e) }); throw e; }
  };
  const cancel = async () => {
    try { if (reader) await reader.cancel(); else if (iterator?.return) await iterator.return(); else if (typeof body.cancel === 'function') await body.cancel(); } catch {}
  };
  return {
    getReader: () => ({ read, cancel, releaseLock() {} }),
    [Symbol.asyncIterator]: () => ({ next: read, return: async () => { await cancel(); return { done: true, value: undefined }; } }),
  };
}

// ── the calibrator ──────────────────────────────────────────────────────────────────────────────
let creating = null; // the trace whose contextFactory call is running
function wrapDeps(tr, deps) {
  const d = { ...deps };
  tr.config.memoryFloorGib = deps.memoryFloorGib ?? 2;
  tr.config.timeouts = clone(deps.timeouts || {});
  if (deps.now) unreplayable(tr, 'now injected'); else d.now = () => tr.clock;
  if (deps.request) d.request = asyncDep(tr, deps.request, (p, opts = {}, timeout) => ({ dep: 'request', path: p, method: opts?.method || 'GET',
    body: parseBody(opts?.body), timeoutMs: timeout == null ? null : Number(timeout), signal: !!opts?.signal }));
  if (deps.rawModels) d.rawModels = asyncDep(tr, deps.rawModels, () => ({ dep: 'rawModels' }));
  if (deps.stream) {
    const realStream = deps.stream;
    d.stream = function (p, opts = {}) {
      const rec = recording() === tr;
      const ev = rec ? begin(tr, { dep: 'stream', path: p, method: opts?.method || 'GET', body: parseBody(opts?.body) }) : null;
      const index = ev ? tr.streams++ : -1;
      let pr;
      try { pr = Promise.resolve(foreign(tr, () => realStream.call(this, p, opts))); } catch (e) { pr = Promise.reject(e); }
      if (!ev) return pr;
      return pr.then(r => {
        settle(tr, ev, { result: { ok: !!r?.ok, status: r?.status ?? null, body: !!r?.body } });
        return r && r.body ? { ok: r.ok, status: r.status, body: wrapBody(tr, r.body, index) } : r;
      }, e => { settle(tr, ev, { throws: thrown(e) }); throw e; });
    };
  }
  for (const [name, arg] of [['conservativeFor', 'model'], ['identityFor', 'model'], ['invalidateBaseline', 'tag']]) {
    const fn = deps[name] || (name === 'invalidateBaseline' ? async () => true : async () => null);
    d[name] = asyncDep(tr, fn, a => ({ dep: name, [arg]: a }));
  }
  const realSleep = deps.sleep || (ms => new Promise(resolve => setTimeout(resolve, ms)));
  d.sleep = function (ms) {
    const rec = recording() === tr;
    const ev = rec ? begin(tr, { dep: 'sleep', ms }) : null;
    let p;
    try { p = Promise.resolve(foreign(tr, () => realSleep.call(this, ms))); } catch (e) { p = Promise.reject(e); }
    return p.then(r => { if (ev) { tr.clock += Math.max(1, Number(ms) || 0); settle(tr, ev, {}); } return r; },
      e => { if (ev) settle(tr, ev, { throws: thrown(e) }); throw e; });
  };
  // A calibrator whose memory interval never fires (memoryPoll >= 1e8) reads memory only from its own
  // code: every read is recorded, in order, and may vary.
  const perCall = Number(deps.timeouts?.memoryPoll) >= 1e8;
  if (deps.readMemory && perCall) {
    const realRead = deps.readMemory;
    tr.config.readMemory = 'calls';
    d.readMemory = function () {
      const v = foreign(tr, () => realRead.apply(this, arguments));
      if (recording() === tr) syncEvent(tr, { dep: 'readMemory', result: v == null ? null : v });
      return v;
    };
  } else if (deps.readMemory) {
    const realRead = deps.readMemory;
    d.readMemory = function () {
      const v = foreign(tr, () => realRead.apply(this, arguments));
      const value = v == null ? null : v;
      if (tr.config.readMemory === undefined) tr.config.readMemory = value;
      else if (tr.config.readMemory !== value) unreplayable(tr, 'readMemory varies');
      return v;
    };
  } else unreplayable(tr, 'readMemory not injected');
  return d;
}

function wrapHooks(tr, hooks) {
  const out = {};
  for (const [name, fn] of Object.entries(hooks)) {
    if (typeof fn !== 'function') { out[name] = fn; continue; }
    const sync = ['contextCurrent', 'onWrite', 'onUpdate'].includes(name);
    const argOf = a => name === 'foreignWait' ? thrown(a) : clone(a === undefined ? null : a);
    out[name] = function (...args) {
      const rec = recording() === tr;
      const base = { dep: 'hook', name, ...(name === 'contextCurrent' ? {} : { arg: argOf(args[0]) }) };
      if (sync) {
        const ev = rec ? syncEvent(tr, base) : null;
        try {
          const r = foreign(tr, () => fn.apply(this, args));
          if (ev) ev.result = name === 'contextCurrent' ? clone(r) ?? null : null;
          return r;
        } catch (e) { if (ev) ev.throws = thrown(e); throw e; }
      }
      const ev = rec ? begin(tr, base) : null;
      let p;
      try { p = Promise.resolve(foreign(tr, () => fn.apply(this, args))); } catch (e) { p = Promise.reject(e); }
      if (!ev) return p;
      return p.then(r => { settle(tr, ev, { result: clone(r) ?? null }); return r; }, e => { settle(tr, ev, { throws: thrown(e) }); throw e; });
    };
  }
  return out;
}

function wrapContext(tr, ctx) {
  const out = { ...ctx };
  for (const [method, fn] of Object.entries(ctx)) {
    if (typeof fn !== 'function') continue;
    out[method] = function (...args) {
      let shown = clone(args);
      if (method === 'compareAndAbort') {
        let current = null;
        try { current = foreign(tr, () => ctx.cancellationFacts().token); } catch {}
        shown = [typeof args[0] === 'string' && args[0] === current ? '<token>' : typeof args[0] === 'string' ? '<stale-token>' : clone(args[0]) ?? null];
      }
      const ev = syncEvent(tr, { dep: 'driver', method, args: shown });
      let r;
      try { r = als.run({ tr, foreign: false }, () => fn.apply(this, args)); } catch (e) { ev.throws = thrown(e); throw e; }
      if (r && typeof r.then === 'function') {
        return r.then(v => { ev.result = clone(v) ?? null; return v; }, e => { ev.throws = thrown(e); throw e; });
      }
      ev.result = clone(r) ?? null;
      return r;
    };
  }
  return out;
}

// ── patches ─────────────────────────────────────────────────────────────────────────────────────
const patched = new WeakSet();
function patchPresets(presets) {
  if (!presets || patched.has(presets)) return;
  patched.add(presets);
  const { get, snapshot, commit } = presets;
  if (get) presets.get = function (model) {
    const r = get.apply(this, arguments); const tr = recording();
    if (tr) syncEvent(tr, { dep: 'presets.get', model, result: clone(r) });
    return r;
  };
  if (snapshot) presets.snapshot = function () {
    const r = snapshot.apply(this, arguments); const tr = recording();
    if (tr) syncEvent(tr, { dep: 'presets.snapshot', result: clone({ text: r?.text, revision: r?.revision }) });
    return r;
  };
  if (commit) presets.commit = function (candidate) {
    const tr = recording();
    if (!tr) return commit.apply(this, arguments);
    const ev = begin(tr, { dep: 'presets.commit', candidate: clone(candidate) ?? null });
    let p;
    try { p = Promise.resolve(foreign(tr, () => commit.apply(this, arguments))); } catch (e) { p = Promise.reject(e); }
    return p.then(r => { settle(tr, ev, { result: clone(r) ?? null }); return r; }, e => { settle(tr, ev, { throws: thrown(e) }); throw e; });
  };
}
function patchMaintenance(m) {
  if (!m || patched.has(m) || typeof m.held !== 'function') return;
  patched.add(m);
  const held = m.held;
  m.held = function () {
    const r = held.apply(this, arguments); const tr = recording();
    if (tr) syncEvent(tr, { dep: 'held', result: clone(r) ?? null });
    return r;
  };
}

const calibration = require(CALIBRATION);
const realCreate = calibration.createCalibrator;
calibration.createCalibrator = function createCalibrator(deps) {
  const tr = creating;
  if (!tr || deps.stateFile !== undefined) return realCreate.call(this, deps);
  creating = null;
  return realCreate.call(this, wrapDeps(tr, deps));
};

const full = require(RUNNER);
const realFull = full.createFullAutotuner;
// The recording contextFactory for a runner's deps (also handed to scenarios.test.cjs, which drives
// private calibrators without a runner).
function recordingFactory(deps) {
  patchPresets(deps.presets);
  patchMaintenance(deps.maintenance);
  const factory = deps.contextFactory;
  return hooks => {
    const callbacks = Object.keys(hooks || {});
    const tr = newTrace(hooks && typeof hooks.contextCurrent === 'function' ? 'context' : 'fill', callbacks);
    const wrapped = wrapHooks(tr, hooks || {});
    creating = tr;
    let ctx;
    try { ctx = als.run({ tr, foreign: false }, () => factory(wrapped)); } finally { if (creating === tr) { creating = null; unreplayable(tr, 'no private calibrator was created'); } }
    return wrapContext(tr, ctx);
  };
}
globalThis.__privateCalibratorFactory = recordingFactory;

// The error-body deadline (llamacpp-calibration.cjs `errorBody`: a real ERROR_BODY_MS setTimeout that
// cancels the read of a rejected long prompt's body). Recorded only when it FIRES, as a sync
// `{ dep: 'tick', ms }` (it lands in the `during` of the read it cut); setting or clearing it is not
// recorded, so a trace where it never fired is unchanged.
globalThis.setTimeout = function setTimeout(cb, ms, ...args) {
  const tr = typeof cb === 'function' ? recording() : null;
  const at = tr ? callerFrame(setTimeout) : null;
  if (!tr || !at || at.file !== CALIBRATION || at.name !== 'errorBody') return real.setTimeout.call(this, cb, ms, ...args);
  return real.setTimeout.call(this, function (...a) { syncEvent(tr, { dep: 'tick', ms: Number(ms) }); return cb.apply(this, a); }, ms, ...args);
};
if (real.setTimeout[require('node:util').promisify.custom]) globalThis.setTimeout[require('node:util').promisify.custom] = real.setTimeout[require('node:util').promisify.custom];
// Scenario hook (scenarios.test.cjs): `ms` of wall time passed while the engine produced a stream
// chunk. Advances the latest trace's fake clock and is recorded as a sync `{ dep: 'clock', ms }` (it
// lands in the `during` of the read in flight).
globalThis.__privateCalibratorElapse = ms => {
  const list = currentTest?.traces, tr = list && list.length ? list[list.length - 1] : null;
  if (!tr || tr.closed) return;
  tr.clock += ms;
  syncEvent(tr, { dep: 'clock', ms });
};
full.createFullAutotuner = function createFullAutotuner(deps) {
  if (!deps || typeof deps.contextFactory !== 'function') return realFull.call(this, deps);
  return realFull.call(this, { ...deps, contextFactory: recordingFactory(deps) });
};

crypto.randomInt = function randomInt(...args) {
  const tr = recording();
  if (tr && callerFile(randomInt) === CALIBRATION && typeof args[0] === 'number' && typeof args[1] === 'number' && args.length === 2) {
    const [min, max] = args, n = ++tr.ints, result = min + ((n * 7919) % (max - min));
    syncEvent(tr, { dep: 'randomInt', min, max, result });
    return result;
  }
  return real.randomInt.apply(crypto, args);
};
crypto.randomUUID = function randomUUID(...args) {
  const tr = recording();
  if (tr && callerFile(randomUUID) === CALIBRATION) {
    const result = '00000000-0000-4000-a000-' + String(++tr.uuids).padStart(12, '0');
    syncEvent(tr, { dep: 'randomUUID', result });
    return result;
  }
  return real.randomUUID.apply(crypto, args);
};

// ── output ──────────────────────────────────────────────────────────────────────────────────────
const slug = name => String(name).toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 60).replace(/-+$/, '') || 'test';
// The calibrator's padding (llamacpp-calibration.cjs PAD): a table string holding a run of it is
// written { prefix, pad: n, suffix } = prefix + PAD.repeat(n) + suffix (lossless).
const PAD = 'This is synthetic padding for a context allocation validation.\n';
function padded(s) {
  const at = s.indexOf(PAD);
  if (at < 0) return s;
  let n = 0;
  while (s.startsWith(PAD, at + n * PAD.length)) n++;
  if (n < 2) return s;
  return { prefix: s.slice(0, at), pad: n, suffix: s.slice(at + n * PAD.length) };
}
function intern(events) {
  const table = [], index = new Map();
  const walk = v => {
    if (typeof v === 'string') {
      if (v.length < 200) return v;
      if (!index.has(v)) { index.set(v, table.length); table.push(padded(v)); }
      return { $s: index.get(v) };
    }
    if (Array.isArray(v)) return v.map(walk);
    if (v && typeof v === 'object') { const o = {}; for (const [k, x] of Object.entries(v)) o[k] = walk(x); return o; }
    return v;
  };
  return { events: walk(events), strings: table };
}
const tmp = require('node:os').tmpdir();
const scrub = text => text.split(tmp + '/').join('<tmp>/');
function write() {
  fs.mkdirSync(OUT, { recursive: true });
  for (const tr of traces) {
    close(tr);
    const { events, strings } = intern(tr.events);
    const head = { version: 1, test: tr.test, instance: tr.instance, kind: tr.kind, callbacks: tr.callbacks, sourceSha256: SOURCES,
      replayable: tr.replayable, why: tr.why, ...(tr.late ? { afterEnd: true } : {}), ...(tr.openAtEnd ? { openAtEnd: tr.openAtEnd } : {}),
      config: clone(tr.config), strings };
    const headText = JSON.stringify(head);
    const body = events.map(e => JSON.stringify(e)).join(',\n');
    const text = headText.slice(0, -1) + ',"events":[\n' + body + '\n]}\n';
    const name = PREFIX + String(tr.ordinal).padStart(3, '0') + '-' + slug(tr.test) + '-' + tr.instance + '.json';
    real.writeFileSync(path.join(OUT, name), scrub(text));
  }
}
process.on('exit', write);
