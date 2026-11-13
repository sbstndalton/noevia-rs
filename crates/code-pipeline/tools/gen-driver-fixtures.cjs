'use strict';
// Records tests/fixtures/code-pipeline-driver.v1.json: noevia-core's createCodePipeline().start
// and its drive(), run end to end against scripted stand-in dependencies, one scenario at a time.
//
//   node crates/code-pipeline/tools/gen-driver-fixtures.cjs <noevia-core>/server > crates/code-pipeline/tests/fixtures/code-pipeline-driver.v1.json
//
// The job store is the REAL jobs.cjs (createJobs over a fresh temporary directory per scenario,
// with a stub clock and fixed job ids), so the lifecycle authority appends, the completeness gate
// before `reviewing` and every derived job (`jobs.get`) are the store's own. The Planner and the
// reviewer are the real createPlannerPlan / createPlannerReview: their in-session path runs over a
// stand-in pinned session. Everything else (workspaces, harness, role engine and its session,
// verifier, approvals, flags, engine key, log, clock) is a recording stub driven by a script.
//
// Per scenario the fixture holds, in order, every call the pipeline made to a dependency:
// `{call, args, beh}`. `args` are the call's arguments with signals, the transport (`connect`),
// `workspaces` and `emit` left out (functions and signals are not data) and a run handle written
// as its number. `beh` is what the stub did, which the Rust replay does the same way:
//   cancel: true   cancel the job first (jobs.cancel) ·  wait: true  wait for the call's signal
//   emit: [[type, data], …]  call the verifier's emit  ·  set: {…}  assign to the run handle
//   handle: {…}    claimRun's new handle (its number is `ret`)
//   ret: value | throw: {message, publicMessage?, result?}
// The real store's calls are recorded the same way, with what the store returned or threw (an
// append records no return value). jobs.run's own appends (`job.started`, the terminal rows) are
// not reachable from outside the store: they are recorded as the `jobs.append` calls they are
// when the store writes their row (a hook on fs.writeFileSync, which jobs.cjs writes each row
// with), so an own append the store refused before writing would go unrecorded (no scenario
// has one). The same hook fails chosen writes (`failWrite(row)`: the write throws
// "QA journal write failed", so jobs.cjs's append throws it). Then `start` (its return or throw),
// the whole journal (type, data, at) and the final job.
//
// Values are written in the tagged JSON form of crates/code-harness/tools/gen-fixtures.cjs:
// {"$js":"undefined"|"NaN"|"Infinity"|"-Infinity"|"-0"}.
const path = require('node:path');
const fs = require('node:fs');
const os = require('node:os');
const crypto = require('node:crypto');

const dir = process.argv[2];
if (!dir) { process.stderr.write('usage: gen-driver-fixtures.cjs <noevia-core>/server\n'); process.exit(2); }
const file = (n) => path.resolve(dir, n);
const FILES = ['code-pipeline.cjs', 'jobs.cjs', 'planner-plan.cjs', 'code-review.cjs', 'role-engine.cjs', 'role-context.cjs',
  'plan-constrained-decoding.cjs', 'code-review-verdict.cjs', 'auditor-report.cjs'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(file(f))).digest('hex');
console.warn = () => {};

// Fixed job ids: jobs.cjs calls crypto.randomUUID() at create time.
let uuidN = 0;
crypto.randomUUID = () => `00000000-0000-4000-8000-${String(++uuidN).padStart(12, '0')}`;

const { createCodePipeline } = require(file('code-pipeline.cjs'));
const { createJobs } = require(file('jobs.cjs'));
const { createPlannerPlan, INSTRUCTIONS: PLANNER_INSTRUCTIONS } = require(file('planner-plan.cjs'));
const { createPlannerReview } = require(file('code-review.cjs'));
const { SHARED_FRAME } = require(file('role-engine.cjs'));
const { DOSSIER_ROLES } = require(file('role-context.cjs'));
const { PLAN_ARTIFACT_SCHEMA } = require(file('plan-constrained-decoding.cjs'));
const { VERDICT_SCHEMA } = require(file('code-review-verdict.cjs'));

// ── Journal writes ──────────────────────────────────────────────────────────────────────────
const WRITE_FAILED = 'QA journal write failed';
const originalWrite = fs.writeFileSync;
let onRow = null;
fs.writeFileSync = function writeFileSync(target, data, ...rest) {
  if (onRow && typeof target === 'number' && typeof data === 'string') onRow(JSON.parse(data));
  return originalWrite.call(this, target, data, ...rest);
};

// ── Tagged values ───────────────────────────────────────────────────────────────────────────
const TAG = '$js';
const own = (o, k, value) => Object.defineProperty(o, k, { value, enumerable: true, writable: true, configurable: true });
function enc(v) {
  if (v === undefined) return { [TAG]: 'undefined' };
  if (typeof v === 'number') {
    if (Number.isNaN(v)) return { [TAG]: 'NaN' };
    if (v === Infinity) return { [TAG]: 'Infinity' };
    if (v === -Infinity) return { [TAG]: '-Infinity' };
    if (Object.is(v, -0)) return { [TAG]: '-0' };
    return v;
  }
  if (v === null || typeof v !== 'object') return v;
  if (Array.isArray(v)) return Array.from(v, enc);
  const out = {};
  for (const k of Object.keys(v)) {
    if (k === TAG) throw Error(`the key ${TAG} is reserved for tags`);
    if (typeof v[k] === 'function') continue;
    own(out, k, enc(v[k]));
  }
  return out;
}
const clone = (v) => (v === undefined ? undefined : structuredClone(v));
const thrownOf = (e) => ({ message: String(e?.message ?? e), ...(e?.publicMessage !== undefined ? { publicMessage: e.publicMessage } : {}),
  ...(e?.result !== undefined ? { result: e.result } : {}) });
const makeError = (t) => Object.assign(Error(t.message), t.publicMessage !== undefined ? { publicMessage: t.publicMessage } : {},
  t.result !== undefined ? { result: clone(t.result) } : {});
const without = (o, keys) => { const out = {}; for (const k of Object.keys(o)) if (!keys.includes(k)) own(out, k, o[k]); return out; };
const DROP = ['signal', 'connect', 'workspaces', 'emit', 'session'];

// ── Synthetic QA data ───────────────────────────────────────────────────────────────────────
const hex = (c) => c.repeat(40);
const BASE = hex('b');
const HEADS = [hex('1'), hex('2'), hex('3'), hex('4'), hex('5')];
const OTHER = hex('e');
const headN = (i) => (i + 1).toString(16).padStart(40, '0');
const PLAN = { goal: 'Add the QA greeting to the QA project', context: ['The QA project prints a greeting.'],
  constraints: ['Keep the change small.'], investigation: ['Read src/qa.js.'],
  steps: [{ n: 1, do: 'Edit src/qa.js to print the QA greeting', done_when: 'The QA greeting is printed' },
    { n: 2, do: 'Add a QA test', done_when: 'The QA test passes' }],
  capabilities: ['read_repository', 'edit_file'], approval_boundaries: ['Every write asks.'], verification: ['Run the QA tests.'],
  completion: 'The QA greeting is printed and tested.', non_goals: ['No other QA files.'] };
const APPROVE = { verdict: 'approve', summary: 'The QA change does what the QA task asks.', findings: [] };
const CHANGES = { verdict: 'request_changes', summary: 'The QA test is missing.',
  findings: [{ severity: 'major', file: 'test/qa.test.js', message: 'Add a QA test for the greeting.' }, { severity: 'minor', message: 'Name the QA constant.' }] };
const WRITE_UP = { completeness: 'complete', summary: 'The QA revision is complete.', evidence: [{ source: 'tests', note: 'The QA tests passed.' }], gaps: [] };
const ok = (v) => ({ ret: { ok: true, text: JSON.stringify(v), corrected: false } });
const report = (a, passed, extra = {}) => ({ kind: 'test-report', passed, exitCode: passed ? 0 : 1, signal: null, timedOut: false,
  tail: passed ? 'QA ok' : '  QA failure: expected greeting\n', tailBytes: 5, totalBytes: 5, truncated: false, taskId: a.taskId,
  headSha: a.headSha, revision: a.revision, repo: a.repo, durationMs: 7, measuredBy: 'verifier', ...extra });
const tests = (args, passed, extra) => {
  const r = report(args[0], passed, extra);
  return { emit: [['step.started', { id: 'tests', title: 'Run the tests' }], ['artifact.created', r], ['step.completed', { id: 'tests', failed: !passed }]],
    ret: { status: passed ? 'passed' : 'failed', report: r, error: null } };
};
const failTests = (extra) => (a) => tests(a, false, extra);
// What the harness's runExecutor leaves: the corrected checkpoint (code-harness.cjs) and the release.
const released = (headSha, more = {}) => (a, st) => ({
  checkpoint: { branch: st.handle.workspace.branch, task: a[1].label.slice(0, 120), baseSha: st.handle.workspace.baseSha ?? null, headSha },
  set: { released: { status: 'released', headSha }, ...more } });

// What each stub does when the scenario's script has nothing queued for it.
const DEFAULTS = {
  'harness.claimRun': () => ({ handle: { workspace: { branch: 'noevia/qa-task', path: '/qa/work/qa-task', baseSha: BASE }, grant: null, cleanupConfirmed: true } }),
  'harness.runExecutor': (a, st) => released(HEADS[st.runs])(a, st),
  'harness.releaseRun': () => ({}),
  'roles.pinModel': () => ({ ret: { ok: true, session: { model: 'qa-model', machine: undefined } } }),
  'session.call:planner': () => ok(PLAN),
  'session.call:reviewer': () => ok(APPROVE),
  'session.call:auditor': () => ok(WRITE_UP),
  'verify.available': () => ({ ret: true }),
  'verify.run': (a) => tests(a, true),
  'workspaces.projectSnapshot': () => ({ ret: { files: ['README.md', 'src/qa.js'], truncated: false, readme: { path: 'README.md', text: 'The QA project.' } } }),
  'workspaces.change': () => ({ ret: { files: [{ path: 'src/qa.js', status: 'M' }, { path: 'test/qa.test.js', status: 'A' }], diff: 'QA diff', truncated: false } }),
  'workspaces.branchTip': (a, st) => ({ ret: st.head }),
  'workspaces.mergePreflight': () => ({ ret: { ok: true, baseBranch: 'main' } }),
  'workspaces.mergeVerified': () => ({ ret: { ok: true, baseBranch: 'main' } }),
  askApproval: () => ({ ret: 'approve' }),
  'flag.merge': () => ({ ret: false }),
  'flag.guard': () => ({ ret: false }),
  'flag.constrain': () => ({ ret: false }),
  'flag.planConstrain': () => ({ ret: false }),
  engineKey: () => ({ ret: 'qa-engine-key-5c1d' }),
  log: () => ({}),
};

async function runScenario(sc) {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-pipeline-'));
  let storeClock = 1_800_000_000_000, clock = 1_900_000_000_000;
  const store = createJobs({ dir: tmp, now: () => (storeClock += 1000) });
  const calls = [];
  const st = { runs: 0, head: null, handle: null, handles: 0, taskId: null };
  const queues = Object.fromEntries(Object.entries(sc.script || {}).map(([k, v]) => [k, [...v]]));
  const behaviour = (name, args) => {
    const q = queues[name];
    const b = q && q.length ? q.shift() : DEFAULTS[name](args, st);
    return typeof b === 'function' ? b(args, st) : b;
  };
  const record = (call, args, beh) => { calls.push({ call, args: enc(args), beh: enc(beh) }); };
  const settle = (b) => { if (b.throw) throw makeError(b.throw); return clone(b.ret); };
  // A stub call: records it, then does what its behaviour says.
  const stubSync = (name, args, key = name) => {
    const b = behaviour(key, args);
    record(name, args, b);
    if (b.set) Object.assign(st.handle, clone(b.set));
    return settle(b);
  };
  const waitFor = (signal) => new Promise((resolve) => { if (!signal || signal.aborted) resolve(); else signal.addEventListener('abort', resolve, { once: true }); });
  async function stubAsync(name, args, { signal, handle, emit, ctx, key = name } = {}) {
    const b = behaviour(key, args);
    record(name, args, b);
    const at = calls.length - 1;
    if (b.cancel) store.cancel(st.taskId);
    if (b.checkpoint) ctx.checkpoint(clone(b.checkpoint));
    if (b.wait) await waitFor(signal);
    for (const [type, data] of b.emit || []) emit(type, clone(data));
    if (b.set) Object.assign(handle || st.handle, clone(b.set));
    if (b.handle) {
      const h = { ...clone(b.handle), id: st.handles += 1 };
      st.handle = h;
      calls[at].beh = enc({ ...b, ret: h.id });
      return h;
    }
    return settle(b);
  }
  // The real store, recorded.
  let depth = 0;
  const real = (call, args, fn, keep = true) => {
    let out;
    depth += 1;
    try { out = fn(); } catch (e) { record(call, args, { throw: thrownOf(e) }); throw e; } finally { depth -= 1; }
    record(call, args, keep ? { ret: clone(out) } : {});
    return out;
  };
  // A row about to be written: jobs.run's own appends are recorded here; chosen writes fail.
  onRow = (row) => {
    const fail = !!(sc.failWrite && sc.failWrite(row));
    if (depth === 0) calls.push({ call: 'jobs.append', args: enc([row.job, row.type, row.data]), beh: fail ? { throw: { message: WRITE_FAILED } } : {} });
    if (fail) throw Error(WRITE_FAILED);
  };
  const journal = (id) => fs.readFileSync(path.join(tmp, 'jobs', `${id}.jsonl`), 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l));
  const runs = [];
  const jobs = {
    create: (a) => { const id = real('jobs.create', [a], () => store.create(a)); st.taskId = id; return id; },
    append: (id, type, data, authority) => (authority
      ? real('jobs.appendAuthority', [id, type, data], () => store.append(id, type, data, authority), false)
      : real('jobs.append', [id, type, data], () => store.append(id, type, data), false)),
    get: (id) => real('jobs.get', [id], () => store.get(id)),
    run: (id, work) => {
      const p = store.run(id, (ctx) => {
        const c = { ...ctx };
        for (const [k, type] of [['event', null], ['checkpoint', 'checkpoint.created'], ['artifact', 'artifact.created']]) {
          c[k] = type ? (data) => real('jobs.append', [id, type, data], () => ctx[k](data), false)
            : (t, data) => real('jobs.append', [id, t, data], () => ctx.event(t, data), false);
        }
        return work(c);
      });
      runs.push(p.catch(() => {}));
      return p;
    },
  };
  const session = (s) => ({ ...s, call: (req) => stubAsync('session.call', without(req, DROP), { signal: req.signal, key: `session.call:${req.role}` }) });
  const log = (entry) => { stubSync('log', [entry]); };
  const roleEngine = {
    pinModel: async (req) => {
      const r = await stubAsync('roles.pinModel', [without(req, DROP)], { signal: req.signal });
      return r && r.ok ? { ...r, session: session(r.session) } : r;
    },
  };
  const deps = {
    jobs,
    workspaces: {
      projectSnapshot: (id) => stubSync('workspaces.projectSnapshot', [id]),
      change: (id) => stubSync('workspaces.change', [id]),
      branchTip: (id) => stubSync('workspaces.branchTip', [id]),
      mergePreflight: (id, o) => stubSync('workspaces.mergePreflight', [id, o]),
      mergeVerified: (id, o) => stubSync('workspaces.mergeVerified', [id, o]),
    },
    harness: {
      claimRun: (req) => stubAsync('harness.claimRun', [req]),
      runExecutor: async (ctx, handle, opts) => {
        try { return await stubAsync('harness.runExecutor', [handle.id, without(opts, DROP)], { signal: ctx.signal, handle, ctx }); }
        finally { st.runs += 1; if (handle.released?.headSha) st.head = handle.released.headSha; }
      },
      releaseRun: (handle) => stubAsync('harness.releaseRun', [handle.id]),
    },
    roleEngine: sc.noRoles ? null : roleEngine,
    planner: createPlannerPlan({ engine: () => ({}), enabled: () => stubSync('flag.planConstrain', []), log, ...(sc.planDeadlineMs ? { deadlineMs: sc.planDeadlineMs } : {}) }),
    review: createPlannerReview({ log, ...(sc.reviewDeadlineMs ? { deadlineMs: sc.reviewDeadlineMs } : {}) }),
    verify: {
      available: () => stubSync('verify.available', []),
      run: (a) => stubAsync('verify.run', [without(a, DROP)], { signal: a.signal, emit: a.emit }),
    },
    askApproval: (request, { signal }) => stubAsync('askApproval', [request], { signal }),
    merge: () => stubSync('flag.merge', []),
    constrain: () => stubSync('flag.constrain', []),
    guard: { enabled: () => stubSync('flag.guard', []) },
    engineKey: () => stubSync('engineKey', []),
    log,
    now: () => { const at = (clock += 1000); record('now', [], { ret: at }); return at; },
    ...(sc.deadlines ? { deadlines: sc.deadlines } : {}),
    ...(sc.maxLoops !== undefined ? { maxLoops: sc.maxLoops } : {}),
    ...(sc.auditWriteUp !== undefined ? { auditWriteUp: sc.auditWriteUp } : {}),
  };
  const pipeline = createCodePipeline(deps);
  const task = { projectId: 'qa-project', repo: { id: 'qa-repo', path: '/qa/repos/qa-repo' }, prompt: 'Add the QA greeting.',
    capabilities: ['read_repository', 'edit_file'], domains: [], tenantId: 'qa-tenant', ...(sc.task || {}) };
  let start;
  try { start = { ret: await pipeline.start({ ...task, ...(sc.noConnect ? {} : { connect: () => {} }) }) }; }
  catch (e) { start = { throw: { ...thrownOf(e), ...(e.status !== undefined ? { status: e.status } : {}) } }; }
  await Promise.all(runs);
  // start's own catch on a run that could not record its start or end runs after that.
  await new Promise((resolve) => { setTimeout(resolve, 20); });
  onRow = null;
  const out = { name: sc.name, task: enc(task), options: enc({ noRoles: !!sc.noRoles, deadlines: sc.deadlines ?? null, maxLoops: sc.maxLoops ?? null,
    auditWriteUp: sc.auditWriteUp ?? null, connect: !sc.noConnect, planDeadlineMs: sc.planDeadlineMs ?? null, reviewDeadlineMs: sc.reviewDeadlineMs ?? null }), start: enc(start), calls };
  if (st.taskId) {
    out.journal = journal(st.taskId).map((e) => ({ type: e.type, at: e.at, data: e.data }));
    out.job = enc(store.get(st.taskId));
  }
  fs.rmSync(tmp, { recursive: true, force: true });
  return out;
}

// ── Scenarios ───────────────────────────────────────────────────────────────────────────────
const MERGE_ON = { 'flag.merge': [{ ret: true }, { ret: true }] };
const LEAK = `QA: ${PLANNER_INSTRUCTIONS.split('\n')[1]}`;
const SCENARIOS = [
  { name: 'happy path, no merge' },
  { name: 'flags on, project instructions, no engine key, machine and grant', task: { context: 'QA project instructions: use two spaces.', model: 'qa-asked-model', harness: 'qa-harness', sandboxKind: 'qa-sandbox', capabilities: ['read_repository', 'edit_file', 'network'], domains: ['qa.example'] },
    script: { 'flag.guard': [{ ret: true }], 'flag.planConstrain': [{ ret: true }], 'flag.constrain': [{ ret: true }], engineKey: [{ ret: null }, { ret: null }],
      'roles.pinModel': [{ ret: { ok: true, session: { model: '', machine: 'qa-box' } } }],
      'harness.claimRun': [{ handle: { workspace: { branch: 'noevia/qa-net', path: '/qa/work/net', baseSha: BASE }, grant: { token: 'qa-grant-token-77' }, cleanupConfirmed: true } }],
      'harness.runExecutor': [released(HEADS[0], { network: { allowed: 3, denied: 1 } })] } },
  { name: 'plan approved', task: { approvePlan: true } },
  { name: 'plan approved for all', task: { approvePlan: true }, script: { askApproval: [{ ret: 'approve_all' }, { ret: 'deny' }] } },
  { name: 'plan denied', task: { approvePlan: true }, script: { askApproval: [{ ret: 'deny' }] } },
  { name: 'change denied', script: { askApproval: [{ ret: 'deny' }] } },
  { name: 'merge: complete audit, preflight ok, merged', script: { ...MERGE_ON } },
  { name: 'merge: recorded failed', script: { ...MERGE_ON, 'workspaces.mergeVerified': [{ ret: { ok: true, baseBranch: 'main', recordFailed: true } }] } },
  { name: 'merge: preflight refused, withheld', script: { ...MERGE_ON, 'workspaces.mergePreflight': [{ ret: { ok: false, code: 'base_checked_out', reason: 'main is checked out in the QA checkout.' } }] } },
  { name: 'merge: audit incomplete, withheld', script: { ...MERGE_ON, 'workspaces.branchTip': [(a, st) => ({ ret: st.head }), { ret: OTHER }] } },
  { name: 'merge: turned off while the card waited', script: { 'flag.merge': [{ ret: true }, { ret: false }] } },
  { name: 'merge: accepted but mergeVerified refused', script: { ...MERGE_ON, 'workspaces.mergeVerified': [{ ret: { ok: false, code: 'base_moved', reason: 'main moved since the QA review.' } }] } },
  { name: 'merge on, change denied', script: { ...MERGE_ON, askApproval: [{ ret: 'deny' }] } },
  { name: 'tests failed, loop, pass', script: { 'verify.run': [failTests({ exitCode: 3, timedOut: true })] } },
  { name: 'request_changes, loop, approve', script: { 'session.call:reviewer': [ok(CHANGES)] } },
  { name: 'loops exhausted, blocked', script: { 'verify.run': [failTests(), failTests(), failTests()] } },
  { name: 'maxLoops 0', maxLoops: 0, script: { 'session.call:reviewer': [ok(CHANGES)] } },
  { name: 'no baseSha', script: { 'harness.claimRun': [{ handle: { workspace: { branch: 'noevia/qa-empty', path: '/qa/work/empty', baseSha: null } } }] } },
  { name: 'short baseSha', script: { 'harness.claimRun': [{ handle: { workspace: { branch: 'noevia/qa-short', path: '/qa/work/short', baseSha: 'abc123' } } }] } },
  { name: 'no role engine', noRoles: true },
  { name: 'pin refused', script: { 'roles.pinModel': [{ ret: { ok: false, code: 'over_budget', reason: 'No QA model fits the budget.' } }] } },
  { name: 'pin throws', script: { 'roles.pinModel': [{ throw: { message: 'The QA admission queue broke.' } }] } },
  { name: 'planner fails', script: { 'session.call:planner': [{ ret: { ok: false, code: 'invalid', reason: 'QA schema violation' } }] } },
  { name: 'planner passes a session failure through', script: { 'session.call:planner': [{ ret: { ok: false, code: 'over_budget', reason: 'The QA model is over budget.' } }] } },
  { name: 'planner unreadable', script: { 'session.call:planner': [{ ret: { ok: true, text: 'not a QA plan' } }] } },
  { name: 'planner invalid plan', script: { 'session.call:planner': [ok({ ...PLAN, capabilities: ['git_push'], steps: [] })] } },
  { name: 'planner throws', script: { 'session.call:planner': [{ throw: { message: 'QA socket closed' } }] } },
  { name: 'brief leak refused', task: { prompt: LEAK } },
  { name: 'executor throws', script: { 'harness.runExecutor': [{ set: { network: { allowed: 1 } }, throw: { message: 'QA agent crashed hard', publicMessage: 'The QA agent crashed.' } }] } },
  { name: 'released not clean', script: { 'harness.runExecutor': [{ set: { released: { status: 'failed', error: 'QA worktree is dirty' } } }] } },
  { name: 'not released', script: { 'harness.runExecutor': [{}] } },
  { name: 'no change', script: { 'harness.runExecutor': [{ set: { released: { status: 'released', headSha: BASE } } }] } },
  { name: 'same head again', script: { 'verify.run': [failTests()], 'harness.runExecutor': [released(HEADS[0]), released(HEADS[0])] } },
  { name: 'cleanup unconfirmed after the run', script: { 'harness.runExecutor': [released(HEADS[0], { cleanupConfirmed: false })] } },
  { name: 'cleanup unconfirmed before resume', script: { 'verify.run': [(a) => ({ ...tests(a, false), set: { cleanupConfirmed: false } })] } },
  { name: 'resume claim throws', script: { 'verify.run': [failTests()], 'harness.claimRun': [DEFAULTS['harness.claimRun'](), { throw: { message: 'The QA branch is gone.' } }] } },
  { name: 'verify unavailable', script: { 'verify.available': [{ ret: false }] } },
  { name: 'verify status unavailable', script: { 'verify.run': [{ ret: { status: 'unavailable', report: null, error: { code: 'no_verifier', message: 'The QA verifier is down.' } } }] } },
  { name: 'verify error', script: { 'verify.run': [{ ret: { status: 'error', report: null, error: { code: 'unmeasured', message: 'The QA report was not measured.' } } }] } },
  { name: 'verify error without a message', script: { 'verify.run': [{ ret: { status: 'odd' } }] } },
  { name: 'verify throws', script: { 'verify.run': [{ throw: { message: 'QA verifier exploded' } }] } },
  { name: 'tests passed without a report, review refused', script: { 'verify.run': [{ ret: { status: 'passed', report: {}, error: null } }] } },
  { name: 'change unreadable', script: { 'workspaces.change': [{ throw: { message: 'QA diff failed' } }] } },
  { name: 'review fails', script: { 'session.call:reviewer': [{ ret: { ok: false, code: 'too_large', reason: 'x' } }] } },
  { name: 'review verdict invalid', script: { 'session.call:reviewer': [ok({ verdict: 'maybe', summary: 'QA', findings: [] })] } },
  { name: 'review throws', script: { 'session.call:reviewer': [{ throw: { message: 'QA reviewer socket' } }] } },
  { name: 'stale verdict, branch tip moved', script: { 'workspaces.branchTip': [{ ret: OTHER }] } },
  { name: 'audit write-up invalid', script: { 'session.call:auditor': [{ ret: { ok: true, text: '{"completeness":"partly"}' } }] } },
  { name: 'audit write-up error', script: { 'session.call:auditor': [{ ret: { ok: false, code: 'timeout', reason: 'QA' } }] } },
  { name: 'audit write-up error without a code', script: { 'session.call:auditor': [{ ret: { ok: false } }] } },
  { name: 'audit write-up off', auditWriteUp: false },
  { name: 'audit call throws', script: { 'session.call:auditor': [{ throw: { message: 'QA auditor down' } }] } },
  { name: 'snapshot throws', script: { 'workspaces.projectSnapshot': [{ throw: { message: 'QA ls-tree failed' } }] } },
  { name: 'empty snapshot, truncated list', script: { 'workspaces.projectSnapshot': [{ ret: { files: [], truncated: false, readme: null } }, { ret: { files: ['a'], truncated: true, readme: null } }] } },
  { name: 'cancel during implement', script: { 'harness.runExecutor': [{ cancel: true, wait: true, throw: { message: 'aborted' } }] } },
  { name: 'cancel during the accept card', script: { askApproval: [{ cancel: true, ret: 'approve' }] } },
  { name: 'cancel during verify, verifier returns', script: { 'verify.run': [(a) => ({ ...tests(a, true), cancel: true })] } },
  { name: 'plan stage deadline', deadlines: { plan: 30 }, script: { 'roles.pinModel': [{ wait: true, throw: { message: 'QA pin abandoned' } }] } },
  { name: 'implement stage deadline', deadlines: { implement: 30 }, script: { 'harness.runExecutor': [{ wait: true, set: { network: { allowed: 2 } }, throw: { message: 'aborted' } }] } },
  { name: 'verify stage deadline, verifier returns late', deadlines: { verify: 30 }, script: { 'verify.run': [(a) => ({ ...tests(a, true), wait: true })] } },
  { name: 'plan deadline (own), session fails late', planDeadlineMs: 30, script: { 'session.call:planner': [{ wait: true, ret: { ok: false, code: 'error', reason: 'QA late' } }] } },
  { name: 'plan deadline (own), session answers late', planDeadlineMs: 30, script: { 'session.call:planner': [{ wait: true, ...ok(PLAN) }] } },
  { name: 'plan cancelled before invalid', script: { 'session.call:planner': [{ cancel: true, ret: { ok: false, code: 'invalid', reason: 'QA' } }] } },
  { name: 'plan aborted code', script: { 'session.call:planner': [{ ret: { ok: false, code: 'aborted', reason: 'QA' } }] } },
  { name: 'plan context refused', script: { 'session.call:planner': [{ ret: { ok: false, code: 'context_refused', reason: 'QA' } }] } },
  { name: 'review deadline (own), good verdict late', reviewDeadlineMs: 30, script: { 'session.call:reviewer': [{ wait: true, ...ok(APPROVE) }] } },
  { name: 'review cancelled, good verdict', script: { 'session.call:reviewer': [{ cancel: true, ...ok(APPROVE) }] } },
  { name: 'review passes an unknown code through', script: { 'session.call:reviewer': [{ ret: { ok: false, code: 'model_mismatch', reason: 'QA other model' } }] } },
  { name: 'review unreadable', script: { 'session.call:reviewer': [{ ret: { ok: true, text: 'QA not json' } }] } },
  { name: 'lone surrogates in the verdict, findings and write-up', script: {
    'session.call:reviewer': [ok({ ...CHANGES, summary: 'QA \uD800 summary', findings: [{ severity: 'major', file: 'src/qa\uDC00.js', message: 'QA \uDBFF message' }] }), ok({ ...APPROVE, summary: 'QA ok \uDFFF' })],
    'session.call:auditor': [ok({ ...WRITE_UP, summary: 'QA audit \uD83D', gaps: ['QA gap \uDE00'] })] } },
  { name: 'lone surrogate in the prompt', task: { prompt: 'Add the QA greeting \uD800 now.' } },
  { name: 'cancel while the pin waits', script: { 'roles.pinModel': [{ cancel: true, wait: true, ret: { ok: true, session: { model: 'qa-model' } } }] } },
  { name: 'cancel on the plan card', task: { approvePlan: true }, script: { askApproval: [{ cancel: true, ret: 'approve' }] } },
  { name: 'tokens across three rounds', maxLoops: 3, script: {
    'harness.claimRun': [1, 2, 3].map((k) => ({ handle: { workspace: { branch: 'noevia/qa-task', path: '/qa/work/qa-task', baseSha: BASE }, grant: { token: `qa-grant-token-${k}` }, cleanupConfirmed: true } })),
    'verify.run': [failTests()], 'session.call:reviewer': [ok(CHANGES), ok(APPROVE)] } },
  { name: 'many rounds reach the stage limit', maxLoops: 30, script: {
    'verify.run': Array.from({ length: 30 }, () => failTests()),
    'harness.runExecutor': Array.from({ length: 30 }, (_, i) => released(headN(i), { network: { allowed: i } })) } },
  { name: 'resume claim from round 3 throws', script: { 'verify.run': [failTests(), failTests()],
    'harness.claimRun': [DEFAULTS['harness.claimRun'](), DEFAULTS['harness.claimRun'](), { throw: { message: 'The QA branch is gone in round 3.' } }] } },
  { name: 'snapshot.files an object', script: { 'workspaces.projectSnapshot': [{ ret: { files: {}, truncated: true, readme: null } }] } },
  { name: 'snapshot.files null', script: { 'workspaces.projectSnapshot': [{ ret: { files: null, readme: null } }] } },
  { name: 'snapshot.files a string', script: { 'workspaces.projectSnapshot': [{ ret: { files: 'QA', readme: null } }] } },
  { name: 'snapshot null', script: { 'workspaces.projectSnapshot': [{ ret: null }] } },
  { name: 'change.files a string', script: { 'workspaces.change': [{ ret: { files: 'QA' } }] } },
  { name: 'change.files with a null entry', script: { 'workspaces.change': [{ ret: { files: [null, { path: 'src/qa.js' }] } }] } },
  { name: 'change paths null and missing', script: { 'workspaces.change': [{ ret: { files: [{ path: null }, { status: 'A' }, { path: 'src/qa.js' }], diff: 'QA diff' } }] } },
  { name: 'verifier answers null', script: { 'verify.run': [{ ret: null }] } },
  { name: 'audit answers null', script: { 'session.call:auditor': [{ ret: null }] } },
  { name: 'merged stage write fails, record-failed note', script: { ...MERGE_ON }, failWrite: (row) => row.type === 'task.stage' && row.data.to === 'merged' },
  { name: 'final job.completed write fails', failWrite: (row) => row.type === 'job.completed' },
  { name: 'final job.completed and job.failed writes fail', failWrite: (row) => row.type === 'job.completed' || row.type === 'job.failed' },
  { name: 'cancel at the release, after the work', script: { 'harness.releaseRun': [{ cancel: true }] } },
  { name: 'cancel at the release, job.cancelled {result} write fails once', script: { 'harness.releaseRun': [{ cancel: true }] },
    failWrite: (() => { let n = 0; return (row) => row.type === 'job.cancelled' && (n += 1) === 1; })() },
  { name: 'cancel on the accept card, job.cancelled write fails', script: { askApproval: [{ cancel: true, ret: 'approve' }] },
    failWrite: (row) => row.type === 'job.cancelled' },
  { name: 'job.failed write fails after a block', script: { 'verify.available': [{ ret: false }] }, failWrite: (row) => row.type === 'job.failed' },
  { name: 'job.started write fails', failWrite: (row) => row.type === 'job.started' },
  { name: 'empty harness string', task: { harness: '' } },
  { name: 'start: no prompt', task: { prompt: '   ' } },
  { name: 'start: no transport', noConnect: true },
  { name: 'start: claim fails', script: { 'harness.claimRun': [{ throw: { message: 'The QA repository is not a git repository.' } }] } },
];

async function main() {
  const scenarios = [];
  for (const sc of SCENARIOS) scenarios.push(await runScenario(sc));
  const constants = { SHARED_FRAME, DOSSIER_ROLES: [...DOSSIER_ROLES], PLAN_ARTIFACT_SCHEMA: JSON.stringify(PLAN_ARTIFACT_SCHEMA), VERDICT_SCHEMA: JSON.stringify(VERDICT_SCHEMA) };
  const source = Object.fromEntries(FILES.map((f) => [f, sha(f)]));
  process.stdout.write(`{"version":1,\n"source":${JSON.stringify(source)},\n"constants":${JSON.stringify(constants)},\n`
    + `"scenarios":[\n${scenarios.map((s) => JSON.stringify(s)).join(',\n')}\n]}\n`);
  process.stderr.write(`${scenarios.length} scenarios, ${scenarios.reduce((n, s) => n + s.calls.length, 0)} calls\n`);
}
const keepAlive = setInterval(() => {}, 1000);
main().then(() => clearInterval(keepAlive), (e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
