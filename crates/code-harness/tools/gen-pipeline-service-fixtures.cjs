'use strict';
// Writes tests/fixtures/pipeline-service-node.v1.json: core's own code-service.cjs (`createCodeService`) with a
// `pipeline` ({ enabled, create }) whose create builds the REAL createCodePipeline over the service's own job store
// (jobs.cjs, the inference lease included) and its own approval cards, with the real createPlannerPlan and
// createPlannerReview; the harness, workspaces, role engine session and verifier are scripted QA stand-ins (no git,
// no sandbox, no model). Each scenario records every answer as routes/code.cjs sends it, the task's journal and the
// log lines. tests/pipeline_service.rs replays it on the Rust service with the same stand-ins.
//
// Usage: node crates/code-harness/tools/gen-pipeline-service-fixtures.cjs <noevia-core>/server > crates/code-harness/tests/fixtures/pipeline-service-node.v1.json
// Synthetic data only: QA names, counted ids, a fixed clock.
//
// Operations: { start: body }; { decide: [task, decision, approval] } (`<T1>` / `<A1>`, `<A2>` = the first task / the
// cards in the order they appeared); { cancel: task }; { facts: true }; { waitCard: task } waits for a card not seen
// before; { waitEnd: task } until the task ended; { startHeld: body } starts without waiting for the answer and waits
// until the harness's claimRun (held in a `holdClaim` scenario) was entered; { releaseClaim: true } lets the claim
// answer and records the start's answer.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');

const serverDir = path.resolve(process.argv[2] || '');
let counter = 0;
const idOf = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
crypto.randomUUID = () => idOf(++counter);
console.warn = () => {};
const { createCodeService, defaultHarnesses } = require(path.join(serverDir, 'code-service.cjs'));
const { createCodePipeline } = require(path.join(serverDir, 'code-pipeline.cjs'));
const { createPlannerPlan } = require(path.join(serverDir, 'planner-plan.cjs'));
const { createPlannerReview } = require(path.join(serverDir, 'code-review.cjs'));

const BASE_SHA = 'b'.repeat(40), HEAD = '1'.repeat(40);
const PLAN = { goal: 'Add the QA greeting to the QA project', context: ['The QA project prints a greeting.'],
  constraints: ['Keep the change small.'], investigation: ['Read src/qa.js.'],
  steps: [{ n: 1, do: 'Edit src/qa.js to print the QA greeting', done_when: 'The QA greeting is printed' }],
  capabilities: ['read_repository', 'edit_file'], approval_boundaries: ['Every write asks.'], verification: ['Run the QA tests.'],
  completion: 'The QA greeting is printed and tested.', non_goals: ['No other QA files.'] };
const ANSWERS = {
  planner: PLAN,
  reviewer: { verdict: 'approve', summary: 'The QA change does what the QA task asks.', findings: [] },
  auditor: { completeness: 'complete', summary: 'The QA revision is complete.', evidence: [{ source: 'tests', note: 'The QA tests passed.' }], gaps: [] },
};
const STANDINS = {
  snapshot: { files: ['README.md', 'src/qa.js'], truncated: false, readme: { path: 'README.md', text: 'The QA project.' } },
  change: { files: [{ path: 'src/qa.js', status: 'M' }], truncated: false },
  workspace: { branch: 'noevia/qa-task', path: '/qa/work/qa-task', baseSha: BASE_SHA },
  head: HEAD,
};

const BODY = { repository: 'qa-repo', prompt: 'QA: add the greeting.', promptPreparation: 'planner' };
const SCENARIOS = {
  'planner-flow': { ops: [{ facts: true }, { start: { ...BODY, approvePlan: true, model: 'qa-model-b' } }, { waitCard: '<T1>' }, { facts: true },
    { decide: ['<T1>', 'approve', '<A1>'] }, { waitCard: '<T1>' }, { decide: ['<T1>', 'approve', '<A2>'] }, { waitEnd: '<T1>' },
    { decide: ['<T1>', 'approve', '<A2>'] }, { facts: true }] },
  'no-plan-card': { ops: [{ start: BODY }, { waitCard: '<T1>' }, { decide: ['<T1>', 'approve_all', '<A1>'] }, { waitEnd: '<T1>' }] },
  'plan-denied': { ops: [{ start: { ...BODY, approvePlan: true } }, { waitCard: '<T1>' }, { decide: ['<T1>', 'deny', '<A1>'] }, { waitEnd: '<T1>' }] },
  'cancel-on-plan-card': { ops: [{ start: { ...BODY, approvePlan: true } }, { waitCard: '<T1>' }, { cancel: '<T1>' }, { waitEnd: '<T1>' },
    { decide: ['<T1>', 'approve', '<A1>'] }, { cancel: '<T1>' }, { facts: true }] },
  'second-start-while-running': { ops: [{ start: { ...BODY, approvePlan: true } }, { waitCard: '<T1>' }, { start: BODY },
    { start: { ...BODY, promptPreparation: 'direct' } }, { cancel: '<T1>' }, { waitEnd: '<T1>' }] },
  'flag-off': { enabled: false, ops: [{ facts: true }, { start: BODY }] },
  'spawn': { sandboxKind: 'spawn', ops: [{ facts: true }, { start: BODY }] },
  'not-set-up': { noCreate: true, ops: [{ facts: true }, { start: BODY }] },
  'paused': { inference: 'paused', ops: [{ start: BODY }] },
  'lease-refused': { inference: 'refuses', ops: [{ start: BODY }, { waitEnd: '<T1>' }] },
  'cancel-during-claim': { holdClaim: true, ops: [{ startHeld: BODY }, { cancel: '<T1>' }, { facts: true }, { releaseClaim: true },
    { waitEnd: '<T1>' }, { cancel: '<T1>' }, { facts: true }] },
};

async function run(sc) {
  counter = 0;
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-pipeline-service-'));
  const logs = [];
  const log = (e) => logs.push(e);
  const inference = sc.inference ? {
    paused: () => sc.inference === 'paused',
    enter: () => { if (sc.inference === 'refuses') throw Object.assign(Error('QA gate held'), { status: 503 }); return () => {}; },
  } : null;
  let hold = null;
  const standins = {
    workspaces: {
      projectSnapshot: () => structuredClone(STANDINS.snapshot), change: () => structuredClone(STANDINS.change),
      branchTip: () => HEAD, mergePreflight: () => ({ ok: true, baseBranch: 'main' }), mergeVerified: () => ({ ok: true, baseBranch: 'main' }),
    },
    harness: {
      claimRun: async (req) => {
        if (sc.holdClaim) await new Promise((release) => { hold = { taskId: req.taskId, release }; });
        return { workspace: { ...STANDINS.workspace }, grant: null, cleanupConfirmed: true };
      },
      runExecutor: async (ctx, handle, opts) => {
        ctx.checkpoint({ branch: handle.workspace.branch, task: String(opts.label).slice(0, 120), baseSha: handle.workspace.baseSha, headSha: HEAD });
        handle.released = { status: 'released', headSha: HEAD };
      },
      releaseRun: async () => null,
    },
    roleEngine: { pinModel: async () => ({ ok: true, session: { model: 'qa-model', call: async (req) => ({ ok: true, text: JSON.stringify(ANSWERS[req.role]), corrected: false }) } }) },
    verify: {
      available: () => true,
      run: async (a) => {
        const report = { kind: 'test-report', passed: true, exitCode: 0, signal: null, timedOut: false, tail: 'QA ok', tailBytes: 5, totalBytes: 5,
          truncated: false, taskId: a.taskId, headSha: a.headSha, revision: a.revision, repo: a.repo, durationMs: 7 };
        a.emit('step.started', { id: 'tests', title: 'Run the tests' });
        a.emit('artifact.created', report);
        a.emit('step.completed', { id: 'tests', failed: false });
        return { status: 'passed', report, error: null };
      },
    },
  };
  const pipeline = {
    enabled: () => sc.enabled !== false,
    create: sc.noCreate ? () => null : ({ jobs, askApproval }) => createCodePipeline({ jobs, askApproval, workspaces: standins.workspaces,
      harness: standins.harness, roleEngine: standins.roleEngine, verify: standins.verify,
      planner: createPlannerPlan({ engine: () => ({}), enabled: () => false, log }), review: createPlannerReview({ log }),
      merge: () => false, constrain: () => false, guard: { enabled: () => false }, engineKey: () => 'qa-engine-key', log, now: () => 5000 }),
  };
  const service = createCodeService({
    repos: [{ id: 'qa-repo', path: '/qa/repos/qa-repo' }], connect: () => {}, egress: null, now: () => 5000, log, timeoutMs: 60000,
    sandboxKind: sc.sandboxKind || 'sandbox', harnesses: defaultHarnesses({ CODE_HARNESS_NAME: 'opencode', CODE_HARNESS_VERSION: '0.0.0-qa' }),
    treeRoot: null, harnessUser: null, sharedContext: () => 'QA shared context.', inference, pipeline,
  });
  const workspace = { dir, userId: 'qa-user' };
  const project = { id: 'qa-project' };
  const route = async (status, fn) => {
    try { return { status, body: await fn() }; }
    catch (error) { return { status: error.status || 500, body: { error: error.publicMessage || (error.status ? error.message : 'Code request failed') } }; }
  };
  const ids = { tasks: [], approvals: [] };
  const sub = (v) => String(v).replace(/<T(\d)>/g, (_, n) => ids.tasks[n - 1] || 'none').replace(/<A(\d)>/g, (_, n) => ids.approvals[n - 1] || 'none');
  const ended = (t) => { const f = path.join(dir, 'jobs', `${t}.jsonl`); return fs.existsSync(f) && /"type":"job\.(completed|failed|cancelled|interrupted)"/.test(fs.readFileSync(f, 'utf8')); };
  const answers = [];
  let pending = null;
  for (const op of sc.ops) {
    if (op.start) {
      const a = await route(202, () => service.start(workspace, project, op.start));
      if (a.body.taskId && !ids.tasks.includes(a.body.taskId)) ids.tasks.push(a.body.taskId);
      answers.push(a);
    } else if (op.startHeld) {
      pending = route(202, () => service.start(workspace, project, op.startHeld));
      for (let i = 0; i < 1000 && !hold; i++) await new Promise((r) => setTimeout(r, 10));
      if (hold && !ids.tasks.includes(hold.taskId)) ids.tasks.push(hold.taskId);
      answers.push({ waited: hold ? 'claiming' : 'none' });
    } else if (op.releaseClaim) {
      if (hold) hold.release();
      const a = await pending;
      if (a.body.taskId && !ids.tasks.includes(a.body.taskId)) ids.tasks.push(a.body.taskId);
      answers.push(a);
    } else if (op.decide) {
      const [t, d, ap] = op.decide.map(sub);
      answers.push(await route(200, () => service.decide(workspace, project, t, d, ap)));
    } else if (op.cancel) {
      answers.push(await route(200, () => service.cancel(workspace, project, sub(op.cancel))));
    } else if (op.facts) {
      answers.push(await route(200, () => service.facts(workspace)));
    } else if (op.waitCard) {
      const t = sub(op.waitCard);
      let card = null;
      for (let i = 0; i < 1000 && !card; i++) {
        const c = service.facts(workspace).approvals[t] || null;
        if (c && !ids.approvals.includes(c.id)) card = c; else await new Promise((r) => setTimeout(r, 10));
      }
      if (card) ids.approvals.push(card.id);
      answers.push({ waited: card ? card.action : 'none' });
    } else if (op.waitEnd) {
      const t = sub(op.waitEnd);
      for (let i = 0; i < 1000 && !ended(t); i++) await new Promise((r) => setTimeout(r, 10));
      await new Promise((r) => setTimeout(r, 50));
      answers.push({ waited: ended(t) ? 'ended' : 'running' });
    }
  }
  const journals = {};
  for (const t of ids.tasks) {
    const f = path.join(dir, 'jobs', `${t}.jsonl`);
    journals[t] = fs.existsSync(f) ? fs.readFileSync(f, 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l)) : null;
  }
  fs.rmSync(dir, { recursive: true, force: true });
  return { enabled: sc.enabled !== false, sandboxKind: sc.sandboxKind || 'sandbox', noCreate: !!sc.noCreate, inference: sc.inference || null,
    holdClaim: !!sc.holdClaim, ops: sc.ops, answers, journals, logs };
}

(async () => {
  const keep = setInterval(() => {}, 1000);
  const sha = (n) => crypto.createHash('sha256').update(fs.readFileSync(path.join(serverDir, n))).digest('hex');
  const out = {};
  for (const [name, sc] of Object.entries(SCENARIOS)) { process.stderr.write(`scenario ${name}\n`); out[name] = await run(sc); }
  clearInterval(keep);
  const sources = Object.fromEntries(['code-service.cjs', 'code-pipeline.cjs', 'jobs.cjs', 'planner-plan.cjs', 'code-review.cjs'].map((n) => [n, sha(n)]));
  process.stdout.write(JSON.stringify({ generator: 'crates/code-harness/tools/gen-pipeline-service-fixtures.cjs', sources,
    standins: { plan: PLAN, answers: ANSWERS, ...STANDINS }, scenarios: out }, null, 1) + '\n', () => process.exit(0));
})();
