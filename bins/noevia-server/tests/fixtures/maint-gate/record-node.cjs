#!/usr/bin/env node
'use strict';
// R6 (sbstndalton/noevia#2641) maintenance-gate recorder: runs noevia-core's own
// server/inference-maintenance.cjs `createMaintenanceGate` with a native lease grant (the
// lose-at-margin lifecycle: nativeHolding/lose, assertCovered, release -> requireRecovery('settle'),
// runRecovery with its backoff) on a virtual clock, and writes what each step observed.
//   node record-node.cjs <core checkout> <out file> <core commit>
// Also writes SOURCES.sha256 beside this file (the core file it exercises, and this recorder itself);
// <core commit> (the checkout's HEAD, read by the caller: the synced tree has no .git) is pinned in
// the fixture. The remote is a scripted
// stub returning native grants ({native, token, askedAt, ttlMs, renewMs, hardMs}); its renewals
// answer true / false / null or are held open ('defer') until a `resolve` step. Timers are virtual:
// the gate's own `setTimer`/`clearTimer`/`now` options, and the global setTimeout/clearTimeout
// (scheduleRecovery uses those) for the duration of a scenario. Synthetic QA data only.
const fs = require('node:fs'), path = require('node:path'), crypto = require('node:crypto');

const [coreArg, outArg, commitArg] = process.argv.slice(2);
if (!coreArg || !outArg || !/^[0-9a-f]{40}$/.test(commitArg || '')) { console.error('usage: record-node.cjs <core checkout> <out file> <core commit>'); process.exit(2); }
const RECORDER = 'tests/fixtures/maint-gate/record-node.cjs';
const recorderSha = crypto.createHash('sha256').update(fs.readFileSync(__filename)).digest('hex');
const core = path.resolve(coreArg);
const SOURCES = ['server/inference-maintenance.cjs'];
const sha = file => crypto.createHash('sha256').update(fs.readFileSync(path.join(core, file))).digest('hex');
const { createMaintenanceGate } = require(path.join(core, 'server', 'inference-maintenance.cjs'));

// ── scenarios ────────────────────────────────────────────────────────────────────────────────
// Times are ms on the virtual clock (0: the work's hold is asked for). `renewals`: the answers of
// the remote's renewMaintenance in call order (then `renewDefault`). `settle`: the settle step's
// outcome per recovery attempt ('ok' | 'fail'; then 'ok'). Steps: hold, assert {stepMs}, release,
// resolve {value} (answers a deferred renewal), require {key, repair: [outcome per run]}, check.
const PROD = { ttlMs: 180000, renewMs: 10000, hardMs: 3600000 };
const SCENARIOS = [
  { name: 'on-time renewals keep a 150 s step covered', grant: PROD, renewals: [], renewDefault: true,
    steps: [{ at: 0, do: 'hold' }, { at: 5000, do: 'assert', stepMs: 150000 }, { at: 95000, do: 'assert', stepMs: 150000 },
      { at: 175000, do: 'assert', stepMs: 150000 }, { at: 200000, do: 'assert', stepMs: 0 }, { at: 200000, do: 'release' },
      { at: 201000, do: 'check' }] },
  { name: 'renewals late inside the margin: the step is still covered', grant: PROD,
    renewals: [null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, 'defer'], renewDefault: true,
    steps: [{ at: 0, do: 'hold' }, { at: 155000, do: 'assert', stepMs: 150000 }, { at: 162000, do: 'assert', stepMs: 150000 },
      { at: 163000, do: 'resolve', value: true }, { at: 175000, do: 'assert', stepMs: 150000 }, { at: 300000, do: 'assert', stepMs: 150000 },
      { at: 300000, do: 'release' }, { at: 301000, do: 'check' }] },
  { name: 'a lapse inside the margin found by the renewal tick', grant: PROD, renewals: [], renewDefault: null,
    steps: [{ at: 0, do: 'hold' }, { at: 160000, do: 'assert', stepMs: 0 }, { at: 165000, do: 'check' },
      { at: 171000, do: 'assert', stepMs: 0 }, { at: 172000, do: 'check' }, { at: 180000, do: 'release' }, { at: 180000, do: 'check' }] },
  { name: 'a lapse inside the margin found by assertCovered before the tick', grant: PROD, renewals: [], renewDefault: null,
    steps: [{ at: 0, do: 'hold' }, { at: 166000, do: 'assert', stepMs: 0 }, { at: 167000, do: 'assert', stepMs: 150000 },
      { at: 168000, do: 'release' }, { at: 168000, do: 'check' }] },
  { name: 'a deferred renewal answered after the margin point: the late answer is ignored', grant: PROD,
    renewals: [null, null, null, null, null, null, null, null, null, null, null, null, null, null, null, 'defer'], renewDefault: true,
    steps: [{ at: 0, do: 'hold' }, { at: 166000, do: 'assert', stepMs: 0 }, { at: 167000, do: 'resolve', value: true },
      { at: 168000, do: 'assert', stepMs: 0 }, { at: 169000, do: 'release' }, { at: 169000, do: 'check' }] },
  { name: 'the hard cap minus the margin', grant: { ttlMs: 180000, renewMs: 10000, hardMs: 60000 }, renewals: [], renewDefault: true,
    steps: [{ at: 0, do: 'hold' }, { at: 10000, do: 'assert', stepMs: 30000 }, { at: 10000, do: 'assert', stepMs: 40000 },
      { at: 10000, do: 'check' }, { at: 44000, do: 'assert', stepMs: 0 }, { at: 46000, do: 'assert', stepMs: 0 },
      { at: 47000, do: 'release' }, { at: 47000, do: 'check' }] },
  { name: 'the hard cap minus the margin found by the renewal tick', grant: { ttlMs: 180000, renewMs: 10000, hardMs: 60000 }, renewals: [], renewDefault: true,
    steps: [{ at: 0, do: 'hold' }, { at: 41000, do: 'assert', stepMs: 0 }, { at: 51000, do: 'assert', stepMs: 0 },
      { at: 52000, do: 'release' }, { at: 52000, do: 'check' }] },
  { name: 'the authority answers that the lease ended', grant: PROD, renewals: [true, true, false], renewDefault: true,
    steps: [{ at: 0, do: 'hold' }, { at: 25000, do: 'assert', stepMs: 150000 }, { at: 31000, do: 'assert', stepMs: 0 },
      { at: 40000, do: 'release' }, { at: 40000, do: 'check' }] },
  { name: 'a recovery that fails, backs off and then succeeds', grant: PROD, renewals: [], renewDefault: null,
    settle: ['fail', 'fail', 'fail'],
    steps: [{ at: 0, do: 'hold' }, { at: 166000, do: 'assert', stepMs: 0 }, { at: 170000, do: 'release' }, { at: 170000, do: 'check' },
      { at: 176000, do: 'check' }, { at: 186000, do: 'check' }, { at: 206000, do: 'check' }, { at: 300000, do: 'check' }] },
  { name: 'the recovery backoff doubles to its cap', grant: PROD, renewals: [], renewDefault: null,
    settle: ['fail', 'fail', 'fail', 'fail', 'fail', 'fail', 'fail'],
    steps: [{ at: 0, do: 'hold' }, { at: 166000, do: 'assert', stepMs: 0 }, { at: 170000, do: 'release' }, { at: 600000, do: 'check' }] },
  // Node's runRecovery releases a failed attempt's hold (which reschedules at 0 ms while recoveries are
  // pending) before it schedules the backoff: a failed settle is retried at once (recorded as it is).
  // A refused fresh hold has no release, so this one records the backoff itself.
  { name: 'a recovery whose fresh hold is refused backs off 5 s doubling to 60 s', grant: PROD, renewals: [], renewDefault: null,
    grants: ['ok', 'refuse', 'refuse', 'refuse', 'refuse', 'refuse', 'refuse', 'ok'],
    steps: [{ at: 0, do: 'hold' }, { at: 166000, do: 'assert', stepMs: 0 }, { at: 170000, do: 'release' }, { at: 170000, do: 'check' },
      { at: 176000, do: 'check' }, { at: 300000, do: 'check' }, { at: 600000, do: 'check' }] },
  { name: 'a repair registered during a lost hold runs after the settle', grant: PROD, renewals: [], renewDefault: null,
    steps: [{ at: 0, do: 'hold' }, { at: 166000, do: 'assert', stepMs: 0 }, { at: 167000, do: 'require', key: 'qa:repair', repair: ['fail', 'ok'] },
      { at: 167000, do: 'check' }, { at: 170000, do: 'release' }, { at: 170000, do: 'check' }, { at: 200000, do: 'check' }] },
  { name: 'an intact hold released owes nothing', grant: PROD, renewals: [], renewDefault: true,
    steps: [{ at: 0, do: 'hold' }, { at: 30000, do: 'release' }, { at: 30000, do: 'check' }, { at: 60000, do: 'check' }] },
];

// ── the virtual clock ────────────────────────────────────────────────────────────────────────
function clock() {
  let t = 0, seq = 0;
  const timers = new Map();
  const add = (fn, ms, every) => { const id = ++seq; timers.set(id, { at: t + Math.max(0, Number(ms) || 0), fn, every }); return { id, unref() {}, ref() {} }; };
  const clear = (h) => { if (h && h.id) timers.delete(h.id); };
  const flush = async () => { for (let i = 0; i < 20; i++) await new Promise((r) => setImmediate(r)); };
  async function advance(to) {
    for (;;) {
      let next = null;
      for (const [id, tm] of timers) if (tm.at <= to && (!next || tm.at < next[1].at || (tm.at === next[1].at && id < next[0]))) next = [id, tm];
      if (!next) break;
      const [id, tm] = next;
      t = tm.at;
      if (tm.every) tm.at += Math.max(1, tm.every); else timers.delete(id);
      tm.fn();
      await flush();
    }
    t = Math.max(t, to);
  }
  return { now: () => t, setTimer: (fn, ms) => add(fn, ms, Math.max(1, ms)), clearTimer: clear, setTimeout: (fn, ms) => add(fn, ms, 0), clearTimeout: clear, advance, flush };
}

async function run(sc) {
  const c = clock();
  const events = [], steps = [];
  const at = () => c.now();
  const renewals = [...sc.renewals];
  const settles = [...(sc.settle || [])];
  let deferred = null, leases = 0, release = null, signal = null;
  const remote = {
    async holdMaintenance(reason) {
      leases += 1;
      const token = `qa-lease-${leases}`;
      const recovery = /recovers/.test(reason || '');
      if ((sc.grants || [])[leases - 1] === 'refuse') {
        events.push({ t: at(), call: 'acquire', refused: true, recovery });
        throw Object.assign(Error('The maintenance lease authority is not answering, so nothing was changed. Try again shortly.'), { status: 503 });
      }
      events.push({ t: at(), call: 'acquire', token, recovery });
      return { token, native: true, askedAt: at(), ttlMs: sc.grant.ttlMs, renewMs: sc.grant.renewMs, hardMs: sc.grant.hardMs, header: false };
    },
    renewMaintenance(token) {
      const answer = renewals.length ? renewals.shift() : sc.renewDefault;
      events.push({ t: at(), call: 'renew', token });
      if (answer === 'defer') return new Promise((resolve) => { deferred = resolve; });
      return Promise.resolve(answer);
    },
    async releaseMaintenance(token) { events.push({ t: at(), call: 'release', token }); },
  };
  const realTimeout = globalThis.setTimeout, realClear = globalThis.clearTimeout;
  globalThis.setTimeout = c.setTimeout; globalThis.clearTimeout = c.clearTimeout;
  try {
    const gate = createMaintenanceGate({ remote, now: c.now, setTimer: c.setTimer, clearTimer: c.clearTimer,
      log: (line) => events.push({ t: at(), log: line }), coverageMarginMs: 15000, recoveryRetryMs: 5000, recoveryMaxDelayMs: 60000 });
    gate.setRecoverySettle(async () => {
      const outcome = settles.length ? settles.shift() : 'ok';
      events.push({ t: at(), settle: outcome });
      if (outcome !== 'ok') throw Error('The model server did not answer.');
    });
    for (const step of sc.steps) {
      await c.advance(step.at);
      const seen = { t: at(), do: step.do };
      if (step.do === 'hold') {
        release = await gate.hold('qa work');
        signal = release.signal || null;
      } else if (step.do === 'assert') {
        seen.stepMs = step.stepMs;
        try { gate.assertCovered(step.stepMs); seen.result = { ok: true }; }
        catch (e) { seen.result = { ok: false, message: e.message, code: e.code || null, status: e.status || null }; }
      } else if (step.do === 'release') {
        release();
      } else if (step.do === 'resolve') {
        if (deferred) { const d = deferred; deferred = null; d(step.value); }
      } else if (step.do === 'require') {
        seen.key = step.key;
        const outcomes = [...step.repair];
        gate.requireRecovery(step.key, async () => {
          const outcome = outcomes.length ? outcomes.shift() : 'ok';
          events.push({ t: at(), repair: step.key, outcome });
          if (outcome !== 'ok') throw Error('The qa repair did not finish.');
        });
      }
      await c.flush();
      seen.held = gate.held();
      seen.recoveryPending = gate.recoveryPending();
      seen.aborted = signal ? signal.aborted : null;
      seen.abortReason = signal && signal.aborted ? (signal.reason?.message ?? null) : null;
      steps.push(seen);
    }
  } finally {
    globalThis.setTimeout = realTimeout; globalThis.clearTimeout = realClear;
  }
  return { name: sc.name, input: { grant: sc.grant, renewals: sc.renewals, renewDefault: sc.renewDefault, settle: sc.settle || [], grants: sc.grants || [], steps: sc.steps }, steps, events };
}

(async () => {
  const scenarios = [];
  for (const sc of SCENARIOS) scenarios.push(await run(sc));
  const sourceSha256 = Object.fromEntries(SOURCES.map((f) => [f, sha(f)]));
  fs.writeFileSync(outArg, JSON.stringify({ version: 1, coreCommit: commitArg, recorderSha256: recorderSha, options: { coverageMarginMs: 15000, recoveryRetryMs: 5000, recoveryMaxDelayMs: 60000 }, sourceSha256, scenarios }, null, 1) + '\n');
  fs.writeFileSync(path.join(__dirname, 'SOURCES.sha256'), SOURCES.map((f) => `${sha(f)}  ${f}\n`).join('') + `${recorderSha}  ${RECORDER}\n`);
})().catch((e) => { console.error(e); process.exit(1); });
