#!/usr/bin/env node
'use strict';
// Recorded throughput-sweep guard behaviour for crates/model-manager-proxy src/sweep_guard.rs: the contract it is
// replayed against (tests/node_sweep_guard.rs).
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/model-manager-proxy/tests/record_sweep_guard.cjs > crates/model-manager-proxy/tests/fixtures/node-sweep-guard.v1.json
//   ... node crates/model-manager-proxy/tests/record_sweep_guard.cjs --check crates/model-manager-proxy/tests/fixtures/node-sweep-guard.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every expectation comes from core's own server/benchmark-sweep-guard.cjs createSweepGuard, run as it is. Stubbed is
// only the outside world, and every stub writes what it was asked into the scenario's trace, in order: hold (a promise
// the driver settles), progress (a promise the driver settles), barrier and its lift, setTimer/clearTimer (fake timers
// the driver fires), log, and the hold's release (with its lease signal).
// The driver is a fixed loop the Rust replay copies: apply the step's events (settling after each), then answer the
// oldest pending call from the scenario's lists, or else fire the oldest live timer; settle; repeat. "Settle" lets every
// microtask run (a few setImmediate turns), so one continuation runs per step and the order is deterministic.
// The inputs are synthetic (hand-made cases from the Node tests plus a seeded PRNG), passed through JSON first so Node
// reads exactly what is recorded. The options Node no longer acts on (perModelMs, slackMs, unreachableMs,
// adoptAttempts, now) are varied and recorded, and the Rust side ignores them. The source file's sha256 is recorded.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const server = path.join(path.resolve(checkout), 'server');
const SOURCE = 'benchmark-sweep-guard.cjs';
const { createSweepGuard, SWEEP_PAUSE_REASON } = require(path.join(server, SOURCE));
const clone = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

// --- acquire's failure mapping ---------------------------------------------------------------------------------

const ERRORS = [
  { status: 503, code: 'maintenanceAuthorityUnavailable' },
  { status: '503', code: 'maintenanceAuthorityUnavailable' },
  { status: 503, code: 'other' },
  { status: 503 },
  { status: 409 },
  { status: 409, fromGate: true },
  { status: 409, chatActive: true },
  { status: 409, chatActive: 1 },
  { status: 502 },
  { status: '502' },
  { fromGate: 'true' },
  {},
  { code: 'maintenanceAuthorityUnavailable' },
  { status: 503, code: 'maintenanceAuthorityUnavailable', fromGate: false, chatActive: true },
];

async function acquireCases() {
  const out = [];
  for (const err of ERRORS) {
    for (const mode of ['sync', 'async']) {
      const e = Object.assign(Error('refused'), clone(err));
      const guard = createSweepGuard({ hold: () => (mode === 'sync' ? (() => { throw e; })() : Promise.reject(e)), progress: async () => null });
      let caught = null;
      try { await guard.acquire(); } catch (x) { caught = x; }
      out.push({ err, mode, failure: { status: caught.status, code: caught.code ?? null, publicMessage: caught.publicMessage,
        message: caught.message, waitable: caught.waitable, unavailable: caught.unavailable === true } });
    }
  }
  return out;
}

// --- scenarios -------------------------------------------------------------------------------------------------

const settle = async () => { for (let i = 0; i < 4; i += 1) await new Promise(setImmediate); };

async function run(sc) {
  const trace = [], pending = [], timers = [], controllers = [], harnessWatches = [];
  const o = sc.options;
  let pi = 0, hi = 0, clock = o.now0, adoptPending = false;
  const makeRelease = (lost) => {
    const c = new AbortController();
    controllers.push(c);
    const release = () => { trace.push('release'); };
    release.signal = c.signal;
    if (lost) c.abort();
    return release;
  };
  const guard = createSweepGuard({
    hold: (reason, options) => {
      trace.push(`hold:${reason === SWEEP_PAUSE_REASON ? 'sweep' : 'other'}:${options?.recovery === true}`);
      return new Promise((resolve, reject) => pending.push({ kind: 'hold', resolve, reject }));
    },
    progress: () => {
      trace.push('progress');
      return new Promise((resolve, reject) => pending.push({ kind: 'progress', resolve, reject }));
    },
    barrier: o.barrier ? (reason) => { trace.push(`barrier:${reason === SWEEP_PAUSE_REASON ? 'sweep' : 'other'}`); return () => trace.push('lift'); } : null,
    log: (m) => trace.push(`log:${m}`),
    pollMs: o.pollMs, perModelMs: o.perModelMs, slackMs: o.slackMs, unreachableMs: o.unreachableMs,
    now: () => { clock += o.nowStep; return clock; },
    setTimer: (fn, ms) => { const t = { fn, ms, live: true }; timers.push(t); trace.push(`timer:${ms}`); return t; },
    clearTimer: (t) => { if (t) t.live = false; trace.push('clear'); },
    adoptRetryMs: o.adoptRetryMs, adoptMaxDelayMs: o.adoptMaxDelayMs, adoptAttempts: o.adoptAttempts,
  });
  const startAdopt = () => {
    if (adoptPending) { trace.push('adopt:skipped'); return; }
    adoptPending = true;
    guard.adopt().then((v) => { adoptPending = false; trace.push(`settled:${v}`); }, () => { adoptPending = false; trace.push('rejected'); });
  };
  const apply = (what) => {
    if (what === 'cancel') guard.cancelAdopt();
    else if (what === 'lose') controllers.at(-1)?.abort();
    else if (what === 'stop') harnessWatches.at(-1)?.stop();
    else if (what === 'watch') harnessWatches.push(guard.watch(makeRelease(false), 1));
    else if (what === 'watch-lost') harnessWatches.push(guard.watch(makeRelease(true), 1));
    else if (what === 'adopt') startAdopt();
    else throw Error(`unknown event ${what}`);
  };
  trace.push(`do:${sc.start}`);
  apply(sc.start);
  await settle();
  let end = 'steps';
  for (let step = 0; step < sc.maxSteps; step += 1) {
    for (const e of sc.events) {
      if (e.at !== step) continue;
      trace.push(`do:${e.do}`);
      apply(e.do);
      await settle();
    }
    if (pending.length) {
      const p = pending.shift();
      if (p.kind === 'progress') {
        if (pi >= sc.progress.length) { end = 'out'; break; }
        const a = sc.progress[pi++];
        trace.push('answer');
        if (a.throws) p.reject(Error('ECONNREFUSED')); else p.resolve(clone(a.value));
      } else {
        if (hi >= sc.holds.length) { end = 'out'; break; }
        const h = sc.holds[hi++];
        trace.push('answer');
        if (h.grant) p.resolve(makeRelease(h.lost === true)); else p.reject(Object.assign(Error('refused'), clone(h.refuse)));
      }
    } else {
      const t = timers.find((x) => x.live);
      if (t) {
        t.live = false;
        trace.push('fire');
        t.fn();
      } else if (sc.events.some((e) => e.at > step)) {
        trace.push('idle');
      } else {
        end = 'quiet';
        break;
      }
    }
    await settle();
  }
  trace.push(`end:${end}`, `active:${guard.active()}`);
  return trace;
}

// Seeded PRNG (mulberry32).
function rng(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const running = (total) => ({ ok: true, body: { job: { active: true, unit: 'models', total } } });
const PROGRESS = [
  { w: 8, a: () => ({ value: running(2) }) },
  { w: 3, a: () => ({ value: running(1) }) },
  { w: 3, a: () => ({ throws: true }) },
  { w: 3, a: () => ({ value: null }) },
  { w: 2, a: () => ({ value: { ok: false, status: 503 } }) },
  { w: 2, a: () => ({ value: { ok: true, body: { job: null } } }) },
  { w: 3, a: () => ({ value: { ok: true, body: { job: { active: false, unit: 'models' } } } }) },
  { w: 1, a: () => ({ value: { ok: true, body: { job: { active: true, unit: 'prompts' } } } }) },
  { w: 1, a: () => ({ value: { ok: true, body: [] } }) },
  { w: 1, a: () => ({ value: { ok: 1, body: { job: { active: true, unit: 'models', total: '3' } } } }) },
  { w: 1, a: () => ({ value: { ok: true, body: { job: true } } }) },
  { w: 1, a: () => ({ value: { ok: true, body: { job: 0 } } }) },
  { w: 1, a: () => ({ value: { ok: true, body: 'x' } }) },
  { w: 1, a: () => ({ value: { ok: true, body: { job: { active: 'true', unit: 'models' } } } }) },
  { w: 1, a: () => ({ value: { ok: true, body: { job: { active: true, unit: 'models', total: [2] } } } }) },
  { w: 1, a: () => ({ value: { ok: true, body: { job: { active: true, unit: 'models', total: { toString: 'x' } } } } }) },
  { w: 1, a: () => ({ value: { ok: true, body: { job: { active: true, unit: 'models' } } } }) },
  { w: 1, a: () => ({ value: 'up' }) },
];
const HOLDS = [
  { w: 4, h: () => ({ grant: true, lost: false }) },
  { w: 1, h: () => ({ grant: true, lost: true }) },
  { w: 3, h: () => ({ refuse: { status: 503, code: 'maintenanceAuthorityUnavailable' } }) },
  { w: 2, h: () => ({ refuse: { status: 409, chatActive: true } }) },
  { w: 1, h: () => ({ refuse: { status: 502 } }) },
];
const EVENTS = ['cancel', 'lose', 'stop', 'watch', 'adopt', 'watch-lost'];
const pickW = (r, list) => {
  const total = list.reduce((s, x) => s + x.w, 0);
  let x = r() * total;
  for (const item of list) { x -= item.w; if (x < 0) return item; }
  return list[list.length - 1];
};
const pick = (r, list) => list[Math.floor(r() * list.length)];

function generated(n) {
  const r = rng(0x5eed14);
  const out = [];
  for (let i = 0; i < n; i += 1) {
    const x = r();
    const start = x < 0.6 ? 'adopt' : x < 0.85 ? 'watch' : 'watch-lost';
    const options = {
      pollMs: pick(r, [1, 1000, 5000]), adoptRetryMs: pick(r, [1, 10, 100, 2000]), adoptMaxDelayMs: pick(r, [2, 150, 60000]),
      barrier: r() < 0.85, perModelMs: pick(r, [1, 5000, 1800000]), slackMs: pick(r, [0, 1000, 300000]),
      unreachableMs: pick(r, [1, 3000, 120000]), adoptAttempts: pick(r, [1, 3, 10]), now0: pick(r, [0, 1e12]), nowStep: pick(r, [0, 1000]),
    };
    const progress = Array.from({ length: 4 + Math.floor(r() * 14) }, () => pickW(r, PROGRESS).a());
    const holds = Array.from({ length: Math.floor(r() * 6) }, () => pickW(r, HOLDS).h());
    const events = Array.from({ length: Math.floor(r() * 3) }, () => ({ at: Math.floor(r() * 16), do: pick(r, EVENTS) }))
      .sort((a, b) => a.at - b.at);
    out.push({ name: `generated ${i}`, options, start, events, progress, holds, maxSteps: 30 });
  }
  return out;
}

const BASE = { pollMs: 1000, adoptRetryMs: 100, adoptMaxDelayMs: 150, barrier: true, perModelMs: 1800000, slackMs: 300000,
  unreachableMs: 120000, adoptAttempts: 10, now0: 0, nowStep: 0 };
const HAND = [
  // benchmark-sweep-guard.test.cjs: an unreachable manager retains exclusion until a terminal answer.
  { name: 'watch: outage then terminal', options: BASE, start: 'watch', events: [],
    progress: [{ value: running(1) }, { value: null }, { value: null }, { value: null }, { value: { ok: true, body: { job: { active: false } } } }], holds: [], maxSteps: 20 },
  { name: 'watch: a throwing progress keeps the hold', options: BASE, start: 'watch', events: [],
    progress: [{ throws: true }, { throws: true }], holds: [], maxSteps: 6 },
  { name: 'watch: a job that is null while watching keeps the hold', options: BASE, start: 'watch', events: [],
    progress: [{ value: { ok: true, body: { job: null } } }, { value: { ok: true, body: { job: null } } }], holds: [], maxSteps: 6 },
  // #895: bounded backoff, then the gate.
  { name: 'adopt: backoff then hold', options: BASE, start: 'adopt', events: [],
    progress: [{ throws: true }, { value: { ok: false, status: 503 } }, { value: { ok: true, body: { job: { active: true, unit: 'models', total: 2 } } } }],
    holds: [{ grant: true }], maxSteps: 12 },
  { name: 'adopt: cancel during a wait keeps the barrier', options: { ...BASE, adoptRetryMs: 10 }, start: 'adopt', events: [{ at: 1, do: 'cancel' }],
    progress: [{ value: null }, { value: null }], holds: [], maxSteps: 6 },
  { name: 'adopt: nothing running', options: BASE, start: 'adopt', events: [], progress: [{ value: { ok: true, body: { job: null } } }], holds: [], maxSteps: 4 },
  // Row J: an outage of the lease authority keeps chat closed while the hold is retried.
  { name: 'adopt: authority outage then grant', options: { ...BASE, adoptRetryMs: 1, adoptMaxDelayMs: 2 }, start: 'adopt', events: [],
    progress: Array.from({ length: 8 }, () => ({ value: { ok: true, body: { job: { active: true, unit: 'models', total: 1 } } } })),
    holds: [...Array.from({ length: 6 }, () => ({ refuse: { status: 503, code: 'maintenanceAuthorityUnavailable' } })), { grant: true }], maxSteps: 40 },
  { name: 'adopt: finished during the outage', options: { ...BASE, adoptRetryMs: 1, adoptMaxDelayMs: 2 }, start: 'adopt', events: [],
    progress: [running(1), running(1), running(1), { ok: true, body: { job: { active: false, unit: 'models' } } }].map((value) => ({ value })),
    holds: Array.from({ length: 4 }, () => ({ refuse: { status: 503, code: 'maintenanceAuthorityUnavailable' } })), maxSteps: 30 },
  { name: 'adopt: without a barrier', options: { ...BASE, barrier: false }, start: 'adopt', events: [],
    progress: [{ value: running(1) }, { value: running(1) }], holds: [{ refuse: { status: 409 } }, { grant: true }], maxSteps: 12 },
  // Delta P1: a lease lost while the sweep runs keeps chat closed until the sweep is over.
  { name: 'watch: lease lost, then over', options: BASE, start: 'watch', events: [{ at: 0, do: 'lose' }, { at: 3, do: 'lose' }],
    progress: [{ value: running(1) }, { value: running(1) }, { value: { ok: true, body: { job: { active: false, unit: 'models', total: 1 } } } }], holds: [], maxSteps: 12 },
  { name: 'adopt: a watch started meanwhile wins', options: BASE, start: 'adopt', events: [{ at: 1, do: 'watch' }],
    progress: [{ value: running(1) }, { value: running(1) }], holds: [{ grant: true }], maxSteps: 8 },
  { name: 'adopt: total that cannot be a number', options: BASE, start: 'adopt', events: [],
    progress: [{ value: { ok: true, body: { job: { active: true, unit: 'models', total: { toString: 'x' } } } } }], holds: [], maxSteps: 4 },
  { name: 'adopt: cancelled for good', options: BASE, start: 'adopt', events: [{ at: 0, do: 'cancel' }, { at: 2, do: 'adopt' }],
    progress: [{ value: null }, { value: null }], holds: [], maxSteps: 6 },
];

async function record() {
  const scenarios = [];
  for (const sc of [...HAND, ...generated(400)]) {
    const input = clone(sc);
    scenarios.push({ ...input, trace: await run(input) });
  }
  const sha = crypto.createHash('sha256').update(fs.readFileSync(path.join(server, SOURCE))).digest('hex');
  return `${JSON.stringify({ version: 1, sources: { [SOURCE]: sha }, acquire: await acquireCases(), scenarios }, null, 1)}\n`;
}

record().then((text) => {
  const check = process.argv.indexOf('--check');
  if (check < 0) { process.stdout.write(text); return; }
  const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
  if (committed !== text) {
    process.stderr.write('The recorded sweep guard differs from the committed fixture: record it again.\n');
    process.exit(1);
  }
  process.stderr.write(`node-sweep-guard fixture is current (${text.length} bytes)\n`);
}, (e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
