'use strict';
// Records the Node tune actor's own answers (the behaviour the five retired bridges exposed) over
// synthetic persisted states, so the Rust owner (crates/server-projects autotune_state.rs) is held to
// it: `cargo test -p server-projects autotune_state::oracle`.
//
//   node record-autotune-owner.cjs --core <dir of noevia-core server/ at the BASE commit> --out <fixture.json>
//
// Base commit used for tests/fixtures/autotune-owner-oracle.json: noevia-core 3521cdc2 (main after #333),
// the last tree in which llamacpp-full-autotune.cjs still owned this state. Synthetic data only; Node 22;
// no engine, model or network access (every engine dependency is a function that fails the run).
const fs = require('node:fs'), os = require('node:os'), path = require('node:path');
const arg = name => { const i = process.argv.indexOf('--' + name); return i > 0 ? process.argv[i + 1] : null; };
const core = path.resolve(arg('core') || ''), out = arg('out');
if (!out || !fs.existsSync(path.join(core, 'llamacpp-full-autotune.cjs'))) { console.error('usage: --core <server dir> --out <file>'); process.exit(2); }
const { createFullAutotuner } = require(path.join(core, 'llamacpp-full-autotune.cjs'));
const { isSystemModel } = require(path.join(core, 'model-system.cjs'));

const job = (status, extra = {}, models) => ({ id: 'j1', model: 'm', status, startedAt: 1000, promptBudgetSeconds: 120, phase: 'x', _revision: 'r1',
  originalText: 'private-text', log: [{ at: 1, text: 'hello' }], queueProgress: { done: 0, total: 1 },
  models: models || [{ model: 'm', status: status === 'running' ? 'running' : 'interrupted', mode: 'fast', kv: ['bf16', 'q8_0'], _identity: 'a'.repeat(64),
    plan: { results: [] }, phases: [
      { id: 'context', label: 'Context', status: 'interrupted', restored: true, steps: [{ id: 'capacity', status: 'passed' }, { id: 'x', status: 'failed', reason: 'oops' }], _beforeText: 'secret' },
      { id: 'sampling', label: 'Sampling', status: 'pending', steps: [{ id: 'apply', status: 'pending' }] },
      { id: 'verify', label: 'Verify', status: 'pending', steps: [] }] }], ...extra });
const history = [{ at: 5, kv: 'bf16', context: 8192, extra: 'dropped', quality: { checks: [{ id: 'a', originalText: 'private-text', answer: 'ok' }] }, sampling: { values: { api_key: 'private-text', temp: 0.7 } }, version: 3, signature: 's' }];

const SCENARIOS = [
  { name: 'empty', state: {} },
  { name: 'null-job', state: { job: null, history: {}, settings: {} } },
  { name: 'running', state: { job: job('running'), history: { m: history, 'm-long': history }, settings: { m: { allowQ5Kv: true } } } },
  { name: 'failed-resumable', state: { job: job('failed', { error: 'boom', failedPhase: 'Context' }), history: { m: history }, settings: {} } },
  { name: 'reload-pending', state: { job: job('interrupted', { _reloadPending: true }), history: {}, settings: { other: { allowQ5Kv: true } } } },
  { name: 'passed', state: { job: job('passed', { finishedAt: 2000 }), history: { m: history }, settings: {} } },
  { name: 'unsafe-and-pending-revision', state: { job: job('interrupted', { _unsafe: true, _pendingRevision: 'r2' }), history: {}, settings: {} } },
  { name: 'wtf16-names', state: JSON.parse('{"job":{"id":"j","model":"m\\ud800","status":"cancelled","models":[],"originalText":"private-text"},"history":{"m\\ud800":[{"at":1,"context":4096}]},"settings":{"m\\ud800":{"allowQ5Kv":true}}}') },
  { name: 'prototype-names', state: JSON.parse('{"history":{"__proto__":[{"at":3,"context":8192}]},"settings":{"__proto__":{"allowQ5Kv":true}}}') },
  { name: 'queue-with-two', state: { job: job('interrupted', {}, [
    { model: 'm', status: 'passed', phases: [{ id: 'context', status: 'passed', steps: [{ status: 'passed' }] }] },
    { model: 'laya', status: 'pending', phases: [{ id: 'context', status: 'pending', steps: [] }] },
    { model: 'n-long', base: 'n', mode: 'long', sectionCreated: true, status: 'interrupted', phases: [{ id: 'context', status: 'interrupted', steps: [{ status: 'running' }] }] }]), history: {}, settings: {} } },
  { name: 'malformed-history', state: { history: { m: { headers: { Authorization: 'private-text' } } }, settings: {} } },
  { name: 'malformed-job', state: { job: [1], history: {}, settings: {} } },
];
const MODELS = ['m', 'm-long', 'laya', 'n', 'n-long', 'other', '__proto__', 'm\ud800'];
const outsideOf = (revision, unconfigured = []) => ({ revision, allowBelowQ5Kv: false,
  models: Object.fromEntries(MODELS.map(n => [n, { configured: !unconfigured.includes(n), system: isSystemModel(n) }])) });
const PLANS = [
  { name: 'launch-all', plan: s => ({ kind: 'launch', reset: s.job.models.flatMap((it, index) => it.status === 'passed' ? [] : [{ index, phases: it.phases.flatMap((p, pi) => p.status === 'passed' ? [] : [{ index: pi, keepSteps: it.plan && ['context', 'verify'].includes(p.id) ? p.steps.flatMap((st, i) => ['passed', 'failed'].includes(st.status) ? [i] : []) : null }]) }]) }) },
  { name: 'launch-bad-index', plan: () => ({ kind: 'launch', reset: [{ index: 9, phases: [] }] }) },
  { name: 'filter-first', plan: s => ({ kind: 'filter', keep: s.job.models.length > 1 ? [0] : [], skipped: s.job.models.slice(1).map((m, i) => ({ index: i + 1, reason: 'System routing model — not tuned' })), complete: s.job.models.length <= 1, model: null }) },
  { name: 'filter-mismatch', plan: () => ({ kind: 'filter', keep: [], skipped: [], complete: false, model: null }) },
  { name: 'nonsense', plan: () => ({ kind: 'nope' }) },
];
const strip = v => JSON.parse(JSON.stringify(v, (k, x) => (k === 'token' || k === 'generation' ? undefined : x)));
const attempt = f => { try { return { ok: strip(f()) }; } catch { return { error: true }; } };
const forbidden = () => { throw new Error('engine access'); };

function actor(file, o) {
  return createFullAutotuner({ stateFile: file, now: () => 123456, request: forbidden, rawModels: forbidden, applyUnlocked: forbidden, identityFor: forbidden, contextFactory: forbidden,
    maintenance: { holdWhenIdle: () => new Promise(() => {}) }, servingChecks: {}, planner: {}, loadAdvisor: {}, longProfiles: {},
    presets: { snapshot: () => ({ revision: o.revision }), get: n => ({ exists: !!o.models[n]?.configured, revision: o.revision }) } });
}
const result = [];
const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'autotune-oracle-'));
try {
  for (const sc of SCENARIOS) {
    const outside = outsideOf('r1');
    const file = path.join(dir, sc.name + '.json');
    const fresh = () => { fs.writeFileSync(file, JSON.stringify(sc.state)); return actor(file, outside); };
    const rec = { name: sc.name, state: sc.state, outside };
    let a;
    try { a = fresh(); } catch { rec.constructs = false; result.push(rec); continue; }
    rec.constructs = true;
    rec.status = attempt(() => a.statusFacts(['m', 'm-long']));
    rec.statusEmpty = attempt(() => a.statusFacts([]));
    rec.statusProto = attempt(() => a.statusFacts(['__proto__', 'constructor']));
    rec.cancel = attempt(() => a.cancellationFacts());
    rec.clear = (() => { const f = strip(a.clearFacts()); const r = attempt(() => a.compareAndClear(a.clearFacts().token)); return { facts: f, result: r, disk: JSON.parse(fs.readFileSync(file, 'utf8')) }; })();
    a = fresh();
    rec.settings = [];
    for (const [model, allow] of [['m', true], ['m', false], ['other', true], ['__proto__', true], ['m\ud800', true]]) {
      a = fresh();
      const entry = { model, allow };
      entry.facts = attempt(() => a.settingsFacts(model));
      entry.commit = attempt(() => { const r = a.commitSettings(model, allow, a.settingsFacts(model).token); return r; });
      entry.disk = JSON.parse(fs.readFileSync(file, 'utf8'));
      rec.settings.push(entry);
    }
    a = fresh();
    rec.resume = { facts: attempt(() => a.resumeFacts()), plans: [] };
    for (const p of PLANS) {
      a = fresh();
      const entry = { name: p.name };
      let plan = null;
      try { plan = p.plan(sc.state); } catch { plan = null; }
      entry.plan = plan;
      if (plan) {
        entry.result = attempt(() => { const r = a.compareAndResume(a.resumeFacts().token, plan); return r; });
        entry.disk = JSON.parse(fs.readFileSync(file, 'utf8'));
      }
      rec.resume.plans.push(entry);
    }
    result.push(rec);
  }
} finally { fs.rmSync(dir, { recursive: true, force: true }); }
fs.writeFileSync(out, JSON.stringify({ base: 'noevia-core 3521cdc2', outsideNote: 'revision r1, every MODELS entry configured', scenarios: result }));
console.log('recorded', result.length, 'scenarios ->', out);
