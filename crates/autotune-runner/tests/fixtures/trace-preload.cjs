'use strict';
// R6 oracle recorder (preload). Loaded with `node --require` before noevia-core's
// server/llamacpp-full-autotune.test.cjs; records every createFullAutotuner instance as one trace
// (format: noevia-work/handoff/tracks/R6-trace-format.md). Read-only on the core checkout.
// Env: TRACE_CORE (core checkout root), TRACE_OUT (output directory).
const fs = require('node:fs'), path = require('node:path'), crypto = require('node:crypto'), util = require('node:util');
const Module = require('node:module');
const { AsyncLocalStorage } = require('node:async_hooks');

const CORE = path.resolve(process.env.TRACE_CORE || '');
const OUT = path.resolve(process.env.TRACE_OUT || '');
if (!process.env.TRACE_CORE || !process.env.TRACE_OUT) throw Error('trace-preload: TRACE_CORE and TRACE_OUT are required');
const SERVER = path.join(CORE, 'server');
const RUNNER = path.join(SERVER, 'llamacpp-full-autotune.cjs');
const TEST = path.join(SERVER, 'llamacpp-full-autotune.test.cjs');
const sha = file => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const SOURCES = { runner: sha(RUNNER), test: sha(TEST) };
const T0 = 1700000000000;
const als = new AsyncLocalStorage();

// ── keep the real primitives before anything else patches them ─────────────────────────────────
const real = {
  writeFileSync: fs.writeFileSync, renameSync: fs.renameSync, randomUUID: crypto.randomUUID,
  setTimeout: globalThis.setTimeout, setImmediate: globalThis.setImmediate, setInterval: globalThis.setInterval,
  clearTimeout: globalThis.clearTimeout, clearInterval: globalThis.clearInterval,
};

function callerFile(fn) {
  const prep = Error.prepareStackTrace, limit = Error.stackTraceLimit;
  try {
    Error.prepareStackTrace = (_, frames) => frames; Error.stackTraceLimit = 1;
    const holder = {}; Error.captureStackTrace(holder, fn);
    const frames = holder.stack;
    return Array.isArray(frames) && frames[0] ? frames[0].getFileName() : null;
  } catch { return null; } finally { Error.prepareStackTrace = prep; Error.stackTraceLimit = limit; }
}

// ── tests: name, ordinal and end of each node:test test ─────────────────────────────────────────
let testOrdinal = 0, currentTest = null, otherUuid = 0;
const traces = [];
function wrapTestFn(info, fn) {
  const wrapped = function (t, ...rest) {
    currentTest = info; otherUuid = 0;
    if (t && typeof t.after === 'function') t.after(() => { for (const tr of info.traces) if (tr.closedAt == null) { tr.closedAt = tr.count; tr.openAtEnd = tr.open.length; } });
    return fn.call(this, t, ...rest);
  };
  // node:test treats a function with two or more parameters as callback style.
  return fn.length >= 2 ? function (t, done) { return wrapped.call(this, t, done); } : wrapped;
}
function wrapTest(realTest) {
  const wrappedTest = function test(...args) {
    const at = args.findIndex(a => typeof a === 'function');
    const name = typeof args[0] === 'string' ? args[0] : (at >= 0 && args[at].name) || '<anonymous>';
    const info = { name, ordinal: ++testOrdinal, instances: 0, traces: [] };
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
Module._load = function (request, parent, isMain) {
  const loaded = realLoad.apply(this, arguments);
  if (request === 'node:test' || request === 'test') return (wrappedNodeTest ||= wrapTest(loaded));
  return loaded;
};

// ── determinism: randomUUID, runner timers ──────────────────────────────────────────────────────
const uuid = (variant, n) => '00000000-0000-4000-' + variant + '-' + String(n).padStart(12, '0');
crypto.randomUUID = function randomUUID() {
  if (callerFile(randomUUID) === RUNNER) {
    const tr = als.getStore();
    if (tr) return uuid('8000', ++tr.uuid);
    return uuid('a000', ++otherUuid);
  }
  return uuid('9000', ++otherUuid);
};
globalThis.setTimeout = function setTimeout(cb, ms, ...args) {
  const file = typeof cb === 'function' ? callerFile(setTimeout) : null;
  if (file === TEST && virtualFor(false)) return vtimer(cb, ms, args, false);
  if (file === RUNNER) {
    const tr = als.getStore();
    const fire = function (...a) { if (tr && tr.closedAt == null) { tr.timers++; unreplayable(tr, 'a runner timer fired (setTimeout)'); emit(tr, { dep: 'timer' }); } return cb.apply(this, a); };
    // A runner timer of up to 1 s (the re-read retry) fires after that many event-loop turns
    // instead of real milliseconds, so where it lands among the test's own setImmediate polling
    // does not depend on the machine's speed. Longer ones (a 120 s deadline) stay real.
    const delay = Number(ms) || 0;
    if (delay <= 1000) return turnTimer(() => fire(...args), Math.max(1, delay));
    return real.setTimeout.call(this, fire, ms, ...args);
  }
  return real.setTimeout.call(this, cb, ms, ...args);
};
if (real.setTimeout[util.promisify.custom]) globalThis.setTimeout[util.promisify.custom] = real.setTimeout[util.promisify.custom];
function turnTimer(fire, turns) {
  const h = { __turnTimer: true, cleared: false, left: turns };
  const step = () => { if (h.cleared) return; if (--h.left > 0) real.setImmediate(step); else { h.cleared = true; fire(); } };
  h.unref = () => h; h.ref = () => h; h.hasRef = () => false;
  h.refresh = () => { if (!h.cleared) h.left = turns; return h; };
  h[Symbol.toPrimitive] = () => -1;
  real.setImmediate(step);
  return h;
}

// The test file's own timers race the run in real time (a cancel 20 ms later, a model that settles
// 2 ms later). While a run of the current test's latest trace is in progress, a test setInterval,
// and a test setTimeout created inside a recorded call (a fake-engine handler), run on that trace's
// fake clock instead: due after that many fake ms, fired when an awaited runner sleep reaches them.
// Outside a run they are real timers. A virtual timer the run never sleeps up to fires on real time
// after its delay plus 1 s (the trace is then marked not replayable).
let vtimerSeq = 0;
const isVTimer = h => h && typeof h === 'object' && h.__vtimer === true;
function latestTrace() { const list = currentTest?.traces; return list && list.length ? list[list.length - 1] : null; }
function vtimer(cb, ms, args, repeat) {
  const tr = latestTrace();
  const vt = { __vtimer: true, id: ++vtimerSeq, cb, ms: Math.max(1, Number(ms) || 0), args, repeat, tr, due: null, real: null, watchdog: null, cleared: false, refed: true };
  vt.unref = () => { vt.refed = false; vt.real?.unref?.(); vt.watchdog?.unref?.(); return vt; };
  vt.ref = () => { vt.refed = true; vt.real?.ref?.(); vt.watchdog?.ref?.(); return vt; };
  vt.hasRef = () => vt.refed;
  vt.refresh = () => { vt.due = null; armVTimer(vt); return vt; };
  vt.close = () => { clearVTimer(vt); return vt; };
  vt[Symbol.toPrimitive] = () => vt.id;
  tr.vtimers.push(vt);
  armVTimer(vt);
  return vt;
}
function disarm(vt) { if (vt.real) real.clearTimeout(vt.real); if (vt.watchdog) real.clearTimeout(vt.watchdog); vt.real = vt.watchdog = null; }
function armVTimer(vt, remaining = null) {
  disarm(vt);
  if (vt.cleared) return;
  const tr = vt.tr;
  if (tr.running && tr.closedAt == null) {
    if (vt.due == null) vt.due = tr.slept + vt.ms;
    vt.watchdog = real.setTimeout(() => { unreplayable(tr, 'a test timer fired on real time during the run'); fireVTimer(vt); }, vt.ms + 1000);
  } else {
    const delay = remaining ?? (vt.due == null ? vt.ms : Math.max(0, vt.due - tr.slept));
    vt.due = null;
    vt.real = real.setTimeout(() => fireVTimer(vt), delay);
  }
  if (!vt.refed) { vt.real?.unref?.(); vt.watchdog?.unref?.(); }
}
function fireVTimer(vt) {
  if (vt.cleared) return;
  disarm(vt);
  if (vt.repeat) { vt.due = vt.due == null ? null : vt.due + vt.ms; armVTimer(vt); }
  else { vt.cleared = true; vt.tr.vtimers = vt.tr.vtimers.filter(x => x !== vt); }
  als.exit(() => vt.cb(...vt.args));
}
function clearVTimer(vt) { vt.cleared = true; disarm(vt); vt.tr.vtimers = vt.tr.vtimers.filter(x => x !== vt); }
function runDueTimers(tr) {
  for (let n = 0; n < 100000; n++) {
    let next = null;
    for (const vt of tr.vtimers) if (vt.due != null && vt.due <= tr.slept && (!next || vt.due < next.due || (vt.due === next.due && vt.id < next.id))) next = vt;
    if (!next) return;
    fireVTimer(next);
  }
}
function setRunning(tr, running) {
  if (tr.running === running) return;
  tr.running = running;
  for (const vt of [...tr.vtimers]) { if (running) vt.due = null; armVTimer(vt); }
}
const virtualFor = repeat => {
  const tr = latestTrace();
  // A setTimeout counts only when made by code running for the trace (a fake-engine handler inside
  // a recorded call), not by the test's own flow.
  return !!tr && tr.closedAt == null && (repeat || (tr.running && als.getStore() === tr));
};
globalThis.setInterval = function setInterval(cb, ms, ...args) {
  if (typeof cb === 'function' && callerFile(setInterval) === TEST && virtualFor(true)) return vtimer(cb, ms, args, true);
  return real.setInterval.call(this, cb, ms, ...args);
};
globalThis.clearTimeout = function clearTimeout(h) {
  if (h && typeof h === 'object' && h.__turnTimer === true) { h.cleared = true; return; }
  return isVTimer(h) ? clearVTimer(h) : real.clearTimeout.call(this, h);
};
globalThis.clearInterval = function clearInterval(h) { return isVTimer(h) ? clearVTimer(h) : real.clearInterval.call(this, h); };

// ── state-file saves ────────────────────────────────────────────────────────────────────────────
const pendingSaves = new Map();
const traceForState = file => {
  const tr = als.getStore();
  if (tr && tr.stateFile === file) return tr;
  for (let i = traces.length - 1; i >= 0; i--) if (traces[i].stateFile === file) return traces[i];
  return null;
};
const saveTarget = tmp => {
  if (typeof tmp !== 'string') return null;
  const dot = tmp.lastIndexOf('.');
  return dot > 0 ? traceForState(tmp.slice(0, dot)) : null;
};
fs.writeFileSync = function writeFileSync(file, data, ...rest) {
  const tr = typeof file === 'string' && callerFile(writeFileSync) === RUNNER ? saveTarget(file) : null;
  if (!tr) return real.writeFileSync.call(this, file, data, ...rest);
  try { const r = real.writeFileSync.call(this, file, data, ...rest); pendingSaves.set(file, { tr, data: String(data) }); return r; }
  catch (e) { emit(tr, { dep: 'save', throws: errorOf(e) }); throw e; }
};
fs.renameSync = function renameSync(from, to, ...rest) {
  const pending = typeof from === 'string' ? pendingSaves.get(from) : null;
  if (!pending) return real.renameSync.call(this, from, to, ...rest);
  pendingSaves.delete(from);
  try { const r = real.renameSync.call(this, from, to, ...rest); emit(pending.tr, { dep: 'save', state: pending.data }); return r; }
  catch (e) { emit(pending.tr, { dep: 'save', throws: errorOf(e) }); throw e; }
};

// ── values ──────────────────────────────────────────────────────────────────────────────────────
const TOKEN_KEYS = new Set(['token', 'generation', 'rowToken', 'preparedToken']);
const CAL_TIME_KEYS = new Set(['startedAt', 'finishedAt', 'at', 'seconds', 'lastSave', 'etaMs', 'updatedAt', 'deadline']);
function clone(value, { calibrator = false } = {}) {
  if (value === undefined) return '<undefined>';
  const seen = new WeakSet();
  const replacer = function (key, v) {
    if (typeof v === 'function') return '<function>';
    if (typeof v === 'bigint') return String(v);
    if (v && typeof v === 'object') {
      if (typeof AbortSignal === 'function' && v instanceof AbortSignal) return '<signal>';
      if (typeof Response === 'function' && v instanceof Response) return '<response>';
      if (v instanceof Error) return errorOf(v);
      if (seen.has(v)) return '<cycle>';
      seen.add(v);
    }
    if (key && TOKEN_KEYS.has(key) && typeof v === 'string') return '<token>';
    if (calibrator && key && CAL_TIME_KEYS.has(key) && typeof v === 'number') return '<time>';
    return v;
  };
  try { const text = JSON.stringify(value, replacer); return text === undefined ? '<undefined>' : JSON.parse(text); }
  catch { return '<unserializable>'; }
}
function errorOf(e) {
  const out = { message: String(e?.message ?? e) }, flags = {};
  if (e && typeof e === 'object') {
    if (e.name && e.name !== 'Error') flags.name = e.name;
    for (const k of Object.keys(e)) {
      if (k === 'message' || k === 'stack') continue;
      const v = e[k];
      if (v === null || ['string', 'number', 'boolean'].includes(typeof v)) flags[k] = v;
    }
  }
  if (Object.keys(flags).length) out.flags = flags;
  return out;
}
const parseBody = body => {
  if (body == null) return null;
  if (typeof body !== 'string') return clone(body);
  try { return JSON.parse(body); } catch { return body; }
};

// ── trace bookkeeping ───────────────────────────────────────────────────────────────────────────
function unreplayable(tr, why) { tr.replayable = false; if (!tr.why.includes(why)) tr.why.push(why); }
function iniText(tr) {
  if (!tr.presets || typeof tr.presets.snapshot !== 'function') return undefined;
  try { const s = tr.presets.snapshot(); return typeof s?.text === 'string' ? s.text : undefined; } catch { return undefined; }
}
function checkIni(tr) {
  if (tr.checkingIni) return;
  tr.checkingIni = true;
  try {
    const text = iniText(tr);
    if (text !== undefined && text !== tr.ini) { tr.ini = text; place(tr, { dep: 'ini', text }); }
  } finally { tr.checkingIni = false; }
}
const EVENT_CAP = 200000;
function place(tr, ev) {
  if (tr.count >= EVENT_CAP) { if (!tr.capped) { tr.capped = true; unreplayable(tr, 'more than ' + EVENT_CAP + ' events; the rest were not recorded'); } return; }
  // After its test ended a trace records nothing more (late timers, recovery retries of a manager
  // whose directory is gone); the trace says that it happened with afterEnd: true.
  if (tr.closedAt != null) { tr.late++; return; }
  if (tr.stopped) return;
  const frame = tr.open[tr.open.length - 1];
  (frame ? (frame.during ||= []) : tr.events).push(ev);
  tr.count++;
}
function emit(tr, ev) { checkIni(tr); place(tr, ev); return ev; }
function openFrame(tr, ev) { emit(tr, ev); tr.open.push(ev); return ev; }
function closeFrame(tr, ev, fields) {
  checkIni(tr);
  const i = tr.open.lastIndexOf(ev);
  if (i >= 0) tr.open.splice(i, 1);
  if (ev.during) { const d = ev.during; delete ev.during; Object.assign(ev, fields); ev.during = d; }
  else Object.assign(ev, fields);
}
// One recorded call: the event goes where the call starts; whatever is recorded before it returns
// is nested in its `during`; result/throws are attached when it settles.
function call(tr, ev, fn, { after = null, resultOf = v => clone(v), sync = false } = {}) {
  openFrame(tr, ev);
  let out;
  try { out = fn(); } catch (e) { closeFrame(tr, ev, { throws: errorOf(e) }); throw e; }
  const settle = v => { const extra = after ? after(v) : {}; closeFrame(tr, ev, { ...(resultOf ? { result: resultOf(v) } : {}), ...extra }); return v; };
  if (!sync && out && typeof out.then === 'function') {
    return Promise.resolve(out).then(settle, e => { closeFrame(tr, ev, { throws: errorOf(e) }); throw e; });
  }
  return settle(out);
}

// ── the wrapper ─────────────────────────────────────────────────────────────────────────────────
const DRIVER = ['start', 'compareAndAbort', 'prepareResume', 'compareAndResume', 'commitSettings', 'compareAndClear', 'recover', 'flushReload', 'scanFacts'];
const isHash = v => typeof v === 'string' && /^[0-9a-f]{64}$/.test(v);
const slug = name => String(name).toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 60).replace(/-+$/, '') || 'test';

function wrapDeps(tr, deps, server) {
  const w = { ...deps };
  const given = k => Object.hasOwn(deps, k) && deps[k] !== undefined;
  const plain = (name, fn, mk) => (...args) => call(tr, mk(...args), () => fn(...args));
  // engine boundary
  if (typeof deps.request === 'function') w.request = (p, opts = {}, timeoutMs) => call(tr,
    { dep: 'request', path: p, method: opts?.method || 'GET', body: parseBody(opts?.body), timeoutMs: timeoutMs ?? '<undefined>' },
    () => deps.request(p, opts, timeoutMs));
  if (typeof deps.rawModels === 'function') w.rawModels = (...args) => call(tr, { dep: 'rawModels' }, () => deps.rawModels(...args));
  if (typeof deps.applyUnlocked === 'function') w.applyUnlocked = (body, writeOptions) => call(tr,
    { dep: 'applyUnlocked', body: clone(body), writeOptions: clone(writeOptions) }, () => deps.applyUnlocked(body, writeOptions),
    { after: () => { const t = iniText(tr); if (t !== undefined) tr.ini = t; return { iniAfter: t ?? null }; } });
  if (typeof deps.identityFor === 'function') w.identityFor = plain('identityFor', deps.identityFor, m => ({ dep: 'identityFor', model: m }));
  w.planFacts = plain('planFacts', given('planFacts') ? deps.planFacts : async () => null, m => ({ dep: 'planFacts', model: m }));
  if (typeof deps.samplingFor === 'function') w.samplingFor = plain('samplingFor', deps.samplingFor, m => ({ dep: 'samplingFor', model: m }));
  w.invalidateBaseline = plain('invalidateBaseline', given('invalidateBaseline') ? deps.invalidateBaseline : async () => true, tag => ({ dep: 'invalidateBaseline', tag }));
  if (typeof deps.reloadRouter === 'function') w.reloadRouter = (...a) => call(tr, { dep: 'reloadRouter' }, () => deps.reloadRouter(...a));
  // onResult: the spec's `result` is the argument's result; the return value is not recorded.
  { const fn = given('onResult') ? deps.onResult : async () => {};
    w.onResult = arg => call(tr, { dep: 'onResult', model: arg?.model, result: clone(arg?.result) }, () => fn(arg), { resultOf: null }); }
  // derived reads, constant per trace
  const realReadMemory = given('readMemory') ? deps.readMemory : require(path.join(server, 'llamacpp-calibration.cjs')).readMemAvailableGib;
  w.readMemory = (...a) => { const v = realReadMemory(...a); observe(tr, 'readMemory', { value: v }); return v; };
  // fileMissing(row): constant per row id; config.fileMissing maps each asked id to its answer.
  if (typeof deps.fileMissing === 'function') w.fileMissing = row => {
    const v = deps.fileMissing(row);
    const id = String(row?.id), map = (tr.config.fileMissing ||= {});
    if (Object.hasOwn(map, id) && map[id] !== !!v) unreplayable(tr, 'fileMissing varied within the trace');
    map[id] = !!v;
    return v;
  };
  const realBudget = given('budgetGib') ? deps.budgetGib : () => 16;
  w.budgetGib = (...a) => { try { const v = realBudget(...a); observe(tr, 'budgetGib', { value: v }); return v; } catch (e) { observe(tr, 'budgetGib', { throws: errorOf(e) }); throw e; } };
  // presets: commit is recorded; get/snapshot/files are derived reads
  if (deps.presets && typeof deps.presets === 'object') {
    const target = deps.presets;
    w.presets = new Proxy(target, { get(t, k) {
      const v = Reflect.get(t, k);
      if (k === 'commit' && typeof v === 'function') return (candidate, options) => call(tr,
        { dep: 'presets.commit', candidate: clone(candidate), options: clone(options) }, () => v.call(t, candidate, options),
        { after: () => { const x = iniText(tr); if (x !== undefined) tr.ini = x; return { iniAfter: x ?? null }; }, resultOf: null });
      return typeof v === 'function' ? v.bind(t) : v;
    } });
  }
  // maintenance
  if (deps.maintenance && typeof deps.maintenance === 'object') {
    const target = deps.maintenance;
    w.maintenance = new Proxy(target, { get(t, k) {
      const v = Reflect.get(t, k);
      if (typeof v !== 'function') return v;
      if (k === 'holdWhenIdle') return (reason, opts = {}) => Promise.resolve(call(tr, { dep: 'holdWhenIdle', reason, timeoutMs: opts?.timeoutMs ?? '<undefined>' },
        () => v.call(t, reason, opts), { resultOf: () => 'held' })).then(release => wrapRelease(tr, release));
      if (k === 'coverageSignal') return (...a) => call(tr, { dep: 'maintenance.coverageSignal', args: clone(a) }, () => v.apply(t, a),
        { resultOf: s => (s == null ? (s === null ? null : '<undefined>') : { aborted: s.aborted === true }), sync: true });
      if (k === 'assertCovered') return (...a) => call(tr, { dep: 'maintenance.assertCovered', args: clone(a) }, () => v.apply(t, a), { sync: true });
      if (k === 'requireRecovery') return (key, repair, message) => call(tr, { dep: 'maintenance.requireRecovery', key, message },
        () => v.call(t, key, typeof repair === 'function' ? wrapCallback(tr, 'maintenance', { name: 'recovery', key }, repair) : repair, message), { sync: true });
      if (k === 'recoverNow') return (...a) => call(tr, { dep: 'maintenance.recoverNow', args: clone(a) }, () => v.apply(t, a));
      return v.bind(t);
    } });
  }
  // context worker
  if (typeof deps.contextFactory === 'function') w.contextFactory = hooks => wrapContext(tr, deps.contextFactory, hooks);
  // decisions (injected or the runner's own default, called the same way)
  const wasm = () => require(path.join(server, 'dav-parse-wasm.cjs'));
  const decide = (fn, injected, request, f) => call(tr, { dep: 'decide', fn, request: clone(request), injected }, f, { sync: fn !== 'judge' });
  const plannerGiven = given('planner'), planner = plannerGiven ? deps.planner : { plan: r => wasm().autotunePlan(r) };
  w.planner = new Proxy(planner, { get(t, k) { const v = Reflect.get(t, k); if (k === 'plan' && typeof v === 'function') return r => decide('plan', plannerGiven, r, () => v.call(t, r)); return typeof v === 'function' ? v.bind(t) : v; } });
  const contentionGiven = given('contention'), contention = contentionGiven ? deps.contention : r => wasm().tuneContention(r);
  w.contention = r => decide('contention', contentionGiven, r, () => contention(r));
  const servingGiven = given('servingChecks');
  const serving = servingGiven ? deps.servingChecks : (() => { const m = wasm(); return { templateCaps: t => m.templateCaps(t), verdict: (s, b) => m.servingVerdict(s, b) }; })();
  w.servingChecks = new Proxy(serving, { get(t, k) {
    const v = Reflect.get(t, k);
    if (k === 'templateCaps' && typeof v === 'function') return tpl => decide('templateCaps', servingGiven, tpl, () => v.call(t, tpl));
    if (k === 'verdict' && typeof v === 'function') return (s, b) => decide('servingVerdict', servingGiven, { status: s, bodyText: b }, () => v.call(t, s, b));
    return typeof v === 'function' ? v.bind(t) : v;
  } });
  const advisorGiven = given('loadAdvisor'), advisor = advisorGiven ? deps.loadAdvisor : require(path.join(server, 'load-advisor.cjs')).createLoadAdvisor();
  w.loadAdvisor = new Proxy(advisor, { get(t, k) { const v = Reflect.get(t, k); if (k === 'judge' && typeof v === 'function') return r => decide('judge', advisorGiven, r, () => v.call(t, r)); return typeof v === 'function' ? v.bind(t) : v; } });
  // The manager always passes one; it is the production default unless a test made its own.
  const longGiven = given('longProfiles') && !defaultLongProfiles.has(deps.longProfiles);
  const long = given('longProfiles') ? deps.longProfiles : require(path.join(server, 'long-profile.cjs')).createLongProfiles();
  w.longProfiles = new Proxy(long, { get(t, k) {
    const v = Reflect.get(t, k);
    if (k === 'pairsOf' && typeof v === 'function') return (rows, presets) => decide('longPairs', longGiven, { rows }, () => v.call(t, rows, presets));
    if (k === 'sectionFor' && typeof v === 'function') return (text, row, presets) => decide('longSection', longGiven, { text, row }, () => v.call(t, text, row, presets));
    return typeof v === 'function' ? v.bind(t) : v;
  } });
  // clock and sleep
  const injectedSleep = given('sleep') ? deps.sleep : null;
  w.now = () => T0 + tr.slept;
  w.sleep = ms => call(tr, { dep: 'sleep', ms }, async () => {
    if (injectedSleep) await injectedSleep(ms); else await new Promise(r => real.setImmediate(r));
    // At least 1 ms per awaited sleep: a zero poll interval must still let a deadline pass.
    tr.slept += Math.max(1, Number(ms) || 0);
    runDueTimers(tr);
  }, { resultOf: null });
  return { wrapped: w, given: { planner: plannerGiven, contention: contentionGiven, servingChecks: servingGiven, loadAdvisor: advisorGiven, longProfiles: longGiven, sleep: !!injectedSleep } };
}
function observe(tr, key, value) {
  const text = JSON.stringify(clone(value));
  if (!(key in tr.derived)) { tr.derived[key] = text; tr.config[key] = 'value' in value ? clone(value.value) : { throws: value.throws }; return; }
  if (tr.derived[key] !== text) unreplayable(tr, key + ' varied within the trace');
}
function wrapRelease(tr, release) {
  if (typeof release !== 'function') return release;
  const r = function (...a) { return call(tr, { dep: 'release' }, () => release.apply(this, a), { resultOf: null, sync: true }); };
  for (const k of Object.keys(release)) r[k] = release[k];
  if (typeof release.setReason === 'function') r.setReason = reason => call(tr, { dep: 'release.setReason', reason }, () => release.setReason(reason), { resultOf: null, sync: true });
  return r;
}
// A callback into the runner: `<scope>.callback` when called, `<scope>.callbackDone` when it settles;
// what the runner records inside appears between the two.
function wrapCallback(tr, scope, fields, fn, { calibrator = false } = {}) {
  return function (...args) {
    // A maintenance callback outside any recorded call or driver call comes from the gate's own
    // retry timer: real time. The first is recorded after a {dep:'timer',source:'maintenance'}
    // marker; how many more come before the test ends is real time, so recording stops at the second.
    if (scope === 'maintenance' && !tr.open.length && !tr.driverDepth && tr.closedAt == null && !tr.stopped) {
      if (tr.gateTimers++ >= 1) { emit(tr, { dep: 'timer', source: 'maintenance', stop: true }); tr.stopped = true; }
      else { emit(tr, { dep: 'timer', source: 'maintenance' }); unreplayable(tr, 'the maintenance gate re-ran a recovery on its own timer'); }
    }
    emit(tr, { dep: scope + '.callback', ...fields, arg: clone(args.length > 1 ? args : args[0], { calibrator }) });
    const done = out => emit(tr, { dep: scope + '.callbackDone', ...fields, ...out });
    let r;
    try { r = als.run(tr, () => fn.apply(this, args)); } catch (e) { done({ throws: errorOf(e) }); throw e; }
    if (r && typeof r.then === 'function') return Promise.resolve(r).then(v => { done({ result: clone(v) }); return v; }, e => { done({ throws: errorOf(e) }); throw e; });
    done({ result: clone(r) });
    return r;
  };
}
function wrapContext(tr, factory, hooks) {
  const id = ++tr.contexts;
  const wrappedHooks = {};
  for (const [k, v] of Object.entries(hooks || {})) wrappedHooks[k] = typeof v === 'function' ? wrapCallback(tr, 'context', { id, name: k }, v, { calibrator: true }) : v;
  emit(tr, { dep: 'context.create', id, callbacks: Object.keys(hooks || {}) });
  const child = factory(wrappedHooks);
  if (!child || typeof child !== 'object') return child;
  const cal = { calibrator: true };
  const out = Object.create(Object.getPrototypeOf(child));
  for (const k of Reflect.ownKeys(child)) Object.defineProperty(out, k, Object.getOwnPropertyDescriptor(child, k));
  const over = (k, mk, opts = {}) => { if (typeof child[k] === 'function') out[k] = (...a) => call(tr, mk(...a), () => child[k](...a), { resultOf: v => clone(v, cal), ...opts }); };
  over('start', (model, opts) => ({ dep: 'context.start', id, model, opts: clone(opts) }));
  over('completion', () => ({ dep: 'context.completion', id }));
  over('status', model => ({ dep: 'context.status', id, model }), { sync: true });
  over('probe', (model, opts) => ({ dep: 'context.probe', id, model, opts: clone(opts) }));
  over('cancelProbe', () => ({ dep: 'context.cancelProbe', id }), { resultOf: null, sync: true });
  over('cancellationFacts', () => ({ dep: 'context.cancellationFacts', id }), { sync: true });
  over('compareAndAbort', token => ({ dep: 'context.compareAndAbort', id, token: typeof token === 'string' ? '<token>' : clone(token) }), { sync: true });
  return out;
}
// Driver calls: tokens are process-salted; an argument token is "<token>" when it equals the
// actor's current one for that call, "<stale-token>" when it is another hash, else kept.
function tokenArg(actor, method, args) {
  const pos = { compareAndAbort: 0, compareAndClear: 0, prepareResume: 0, compareAndResume: 0, commitSettings: 2 }[method];
  if (pos == null || !isHash(args[pos])) return;
  let current = null;
  try {
    current = method === 'compareAndAbort' ? actor.cancellationFacts().token
      : method === 'compareAndClear' ? actor.clearFacts().token
        : method === 'commitSettings' ? actor.settingsFacts(args[0]).token : actor.resumeFacts().token;
  } catch { current = null; }
  args[pos] = args[pos] === current ? '<token>' : '<stale-token>';
}
function wrapActor(tr, actor) {
  const out = { ...actor };
  for (const m of DRIVER) {
    if (typeof actor[m] !== 'function') continue;
    out[m] = (...args) => {
      const shown = args.map(a => a);
      tokenArg(actor, m, shown);
      const ev = { dep: 'driver', method: m, args: clone(shown) };
      emit(tr, ev);
      const settle = fields => Object.assign(ev, fields);
      const launching = m === 'start' || (m === 'compareAndResume' && args[1]?.kind === 'launch');
      const wasRunning = tr.running;
      if (launching) setRunning(tr, true);
      let r;
      tr.driverDepth++;
      try { r = als.run(tr, () => actor[m](...args)); } catch (e) { if (launching) setRunning(tr, wasRunning); settle({ throws: errorOf(e) }); throw e; }
      finally { tr.driverDepth--; }
      const launched = v => {
        if ((m === 'start' && v?.ok) || (m === 'compareAndResume' && v?.applied && args[1]?.kind === 'launch')) watchRun(tr, actor, m);
        else if (launching) setRunning(tr, wasRunning);
      };
      if (r && typeof r.then === 'function') return r.then(v => { settle({ result: clone(v) }); launched(v); return v; }, e => { if (launching) setRunning(tr, wasRunning); settle({ throws: errorOf(e) }); throw e; });
      settle({ result: clone(r) }); launched(r);
      return r;
    };
  }
  return out;
}
// driverAwait: the run a driver call launched has settled. Recorded when it settles (not when the
// test notices), so its place does not depend on the test's polling.
function watchRun(tr, actor, method) {
  const run = ++tr.runs;
  const done = () => { if (tr.runs === run) { emit(tr, { dep: 'driverAwait', method }); setRunning(tr, false); } };
  Promise.resolve(actor.completion()).then(done, done);
}

const defaultLongProfiles = new WeakSet();
function install() {
  const longModule = require(path.join(SERVER, 'long-profile.cjs')), realLong = longModule.createLongProfiles;
  longModule.createLongProfiles = function createLongProfiles(...a) { const made = realLong.apply(this, a); if (made && typeof made === 'object' && !a[0]?.decide) defaultLongProfiles.add(made); return made; };
  const runnerModule = require(RUNNER);
  const realCreate = runnerModule.createFullAutotuner;
  runnerModule.createFullAutotuner = function createFullAutotuner(deps = {}) {
    const info = currentTest || { name: '<outside a test>', ordinal: 0, instances: 0, traces: [] };
    const instance = info.instances++;
    let initialState = null;
    try { initialState = typeof deps.stateFile === 'string' ? fs.readFileSync(deps.stateFile, 'utf8') : null; } catch { initialState = null; }
    const tr = { test: info.name, ordinal: info.ordinal, instance, stateFile: deps.stateFile, presets: deps.presets,
      events: [], open: [], count: 0, late: 0, closedAt: null, uuid: 0, slept: 0, contexts: 0, timers: 0, runs: 0, running: false, vtimers: [], stopped: false, gateTimers: 0, driverDepth: 0,
      replayable: true, why: [], derived: {}, config: {}, ini: undefined, checkingIni: false };
    tr.ini = iniText(tr);
    tr.initial = { state: initialState, ini: tr.ini ?? null };
    info.traces.push(tr); traces.push(tr);
    const { wrapped, given } = wrapDeps(tr, deps, SERVER);
    const env = String(process.env.NOEVIA_AUTOTUNE_ALLOW_BELOW_Q5_KV || '').toLowerCase();
    Object.assign(tr.config, {
      betweenModelsMs: deps.betweenModelsMs ?? 1000, idleTimeoutMs: deps.idleTimeoutMs ?? 300000, memoryFloorGib: deps.memoryFloorGib ?? 2,
      foreignWaitMs: deps.foreignWaitMs ?? 900000, foreignQuietMs: deps.foreignQuietMs ?? 30000, foreignPollMs: deps.foreignPollMs ?? 2000,
      servicesReserveMib: deps.servicesReserveMib ?? 2560, allowBelowQ5Kv: ['1', 'true', 'yes', 'on'].includes(env),
      samplingFor: typeof deps.samplingFor === 'function', planner: given.planner ? 'injected' : 'wasm',
      contention: given.contention ? 'injected' : 'wasm', servingChecks: given.servingChecks ? 'injected' : 'wasm',
      loadAdvisor: given.loadAdvisor ? 'injected' : 'default', longProfiles: given.longProfiles ? 'injected' : 'default',
      routerReload: typeof deps.reloadRouter === 'function', contextFactory: typeof deps.contextFactory === 'function',
      maintenance: deps.maintenance && typeof deps.maintenance === 'object' ? Object.keys(deps.maintenance).filter(k => typeof deps.maintenance[k] === 'function').sort() : null,
      sleep: given.sleep ? 'injected' : 'default',
    });
    const actor = als.run(tr, () => realCreate(wrapped));
    return wrapActor(tr, actor);
  };
}
install();

// ── output ──────────────────────────────────────────────────────────────────────────────────────
const TMP = fs.realpathSync(require('node:os').tmpdir());
function scrub(text) {
  return text.split(TMP + '/').join('<tmp>/');
}
// Size: in preorder (the order events happened), a save after the first is written as a delta of
// the previous save's text (UTF-8 bytes; both cut points on character boundaries), and a
// rawModels/request result.body equal to the previous one for the same path as {"same": true}.
// Myers diff over JSON-ish tokens; `edits` are [keep, delete, insert]: copy `keep` bytes of the
// previous text, skip `delete` bytes, write `insert`; the rest of the previous text follows.
const tokens = text => text.match(/[^,{}\[\]:"]+|[,{}\[\]:"]/g) || [];
function myers(a, b, maxD) {
  const n = a.length, m = b.length, max = n + m, off = max + 1;
  let v = new Int32Array(2 * max + 3); const trace = [];
  for (let d = 0; d <= Math.min(max, maxD); d++) {
    trace.push(v.slice(Math.max(0, off - d - 1), off + d + 2));
    for (let k = -d; k <= d; k += 2) {
      let x = k === -d || (k !== d && v[off + k - 1] < v[off + k + 1]) ? v[off + k + 1] : v[off + k - 1] + 1;
      let y = x - k;
      while (x < n && y < m && a[x] === b[y]) { x++; y++; }
      v[off + k] = x;
      if (x >= n && y >= m) return backtrack(trace, a, b, d, off);
    }
  }
  return null;
}
function backtrack(trace, a, b, dEnd, off) {
  // trace[d] holds v[off-d-1 .. off+d+1] as it was before round d.
  const get = (d, k) => trace[d][k + d + 1];
  const ops = []; let x = a.length, y = b.length;
  for (let d = dEnd; d > 0; d--) {
    const k = x - y;
    const down = k === -d || (k !== d && get(d, k - 1) < get(d, k + 1));
    const pk = down ? k + 1 : k - 1, px = get(d, pk), py = px - pk;
    while (x > px + (down ? 0 : 1) && y > py + (down ? 1 : 0)) { ops.push(['=', a[x - 1]]); x--; y--; }
    if (down) { ops.push(['+', b[y - 1]]); y--; } else { ops.push(['-', a[x - 1]]); x--; }
  }
  while (x > 0 && y > 0) { ops.push(['=', a[x - 1]]); x--; y--; }
  return ops.reverse();
}
function delta(prev, next) {
  const a = tokens(prev), b = tokens(next);
  if (a.join('') !== prev || b.join('') !== next) return null;
  // Common prefix/suffix first, so the diff only sees the changed middle.
  let p = 0; while (p < a.length && p < b.length && a[p] === b[p]) p++;
  let q = 0; while (q < a.length - p && q < b.length - p && a[a.length - 1 - q] === b[b.length - 1 - q]) q++;
  const ops = myers(a.slice(p, a.length - q), b.slice(p, b.length - q), 400);
  if (!ops) return null;
  const edits = []; let keep = a.slice(0, p).reduce((n, t) => n + Buffer.byteLength(t), 0), del = 0, ins = '';
  const flush = () => { if (del || ins) { edits.push([keep, del, ins]); keep = 0; del = 0; ins = ''; } };
  for (const [op, t] of ops) {
    if (op === '=') { if (del || ins) flush(); keep += Buffer.byteLength(t); }
    else if (op === '-') del += Buffer.byteLength(t);
    else ins += t;
  }
  flush();
  return edits;
}
function applyDelta(prev, edits) {
  const a = Buffer.from(prev, 'utf8'), out = []; let at = 0;
  for (const [keep, del, ins] of edits) { out.push(a.subarray(at, at + keep)); at += keep + del; out.push(Buffer.from(ins, 'utf8')); }
  out.push(a.subarray(at));
  return Buffer.concat(out).toString('utf8');
}
const LONG_STRING = 2048;
function compact(events) {
  let lastSave = null; const lastBody = new Map();
  // Every string of 2048+ characters that occurs more than once in the events' values (not in
  // save texts) is written once in `strings` and as {"$string": i} wherever it occurs.
  const counts = new Map(), table = [], index = new Map();
  const count = value => {
    if (typeof value === 'string') { if (value.length >= LONG_STRING) counts.set(value, (counts.get(value) || 0) + 1); }
    else if (Array.isArray(value)) value.forEach(count);
    else if (value && typeof value === 'object') Object.values(value).forEach(count);
  };
  const intern = value => {
    if (typeof value === 'string') {
      if (value.length < LONG_STRING || counts.get(value) < 2) return value;
      if (!index.has(value)) index.set(value, table.push(value) - 1);
      return { $string: index.get(value) };
    }
    if (Array.isArray(value)) return value.map(intern);
    if (value && typeof value === 'object') { const o = {}; for (const [k, v] of Object.entries(value)) o[k] = intern(v); return o; }
    return value;
  };
  const valueKeys = ev => Object.keys(ev).filter(k => !['dep', 'during', 'state', 'delta'].includes(k));
  const visit = list => {
    for (const ev of list) {
      if (ev.dep === 'save' && typeof ev.state === 'string') {
        if (lastSave !== null) {
          const d = delta(lastSave, ev.state);
          if (d && applyDelta(lastSave, d) !== ev.state) throw Error('trace-preload: save delta does not reproduce the state');
          if (d && JSON.stringify(d).length + 16 < ev.state.length) { lastSave = ev.state; delete ev.state; ev.delta = d; }
          else lastSave = ev.state;
        } else lastSave = ev.state;
      }
      if ((ev.dep === 'rawModels' || ev.dep === 'request') && ev.result && typeof ev.result === 'object' && 'body' in ev.result) {
        const key = ev.dep === 'rawModels' ? 'rawModels' : 'request ' + ev.path, text = JSON.stringify(ev.result.body);
        if (lastBody.get(key) === text) ev.result.body = { same: true };
        lastBody.set(key, text);
      }
      if (ev.during) visit(ev.during);
    }
  };
  visit(events);
  const each = (list, fn) => { for (const ev of list) { fn(ev); if (ev.during) each(ev.during, fn); } };
  each(events, ev => { for (const k of valueKeys(ev)) count(ev[k]); });
  each(events, ev => { for (const k of valueKeys(ev)) ev[k] = intern(ev[k]); });
  return table;
}
function write() {
  fs.mkdirSync(OUT, { recursive: true });
  for (const tr of traces) {
    const strings = compact(tr.events);
    const head = { version: 1, test: tr.test, instance: tr.instance, sourceSha256: SOURCES, replayable: tr.replayable,
      ...(tr.replayable ? {} : { why: tr.why }), ...(tr.late ? { afterEnd: true } : {}), ...(tr.openAtEnd ? { openAtEnd: tr.openAtEnd } : {}),
      config: tr.config, initial: tr.initial, ...(strings.length ? { strings } : {}) };
    const headText = JSON.stringify(head);
    const body = tr.events.map(e => JSON.stringify(e)).join(',\n');
    const text = headText.slice(0, -1) + ',"events":[\n' + body + '\n]}\n';
    const name = String(tr.ordinal).padStart(3, '0') + '-' + slug(tr.test) + '-' + tr.instance + '.json';
    real.writeFileSync(path.join(OUT, name), scrub(text));
  }
}
process.on('exit', write);
