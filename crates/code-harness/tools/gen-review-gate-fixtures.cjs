'use strict';
// Writes tests/fixtures/review-gate-node.v1.json: direct Code tasks with the Planner review on
// (features.plannerReview, #519), run by noevia-core's own code-harness.cjs `start` (jobs.cjs store,
// code-workspace.cjs worktree, code-acp.cjs over TCP against a scripted fake sandbox) with the
// reviewer index.cjs builds: code-review.cjs `createPlannerReview` over a scripted role engine
// (`pinModel({ taskId })` and the pinned session's `call`). Recorded per scenario: the journal file,
// the approval cards (the `review_change` card among them), the harness log, the reviewer's own log,
// every pin and call the role engine saw (the bounded review state), and what the sandbox received.
// tests/review_gate.rs runs the same scenarios through the Rust executor and compares.
//
// Usage: node crates/code-harness/tools/gen-review-gate-fixtures.cjs <noevia-core>/server > crates/code-harness/tests/fixtures/review-gate-node.v1.json
// Synthetic data only: a QA repository, fixed ids, a fixed clock, a fake engine address.
//
// A scenario: `script` (the fake sandbox, as gen-executor-fixtures.cjs), `answers` (the cards, in
// order; 'deny' after), `capabilities`, the sandbox's `{ commit: true }` (git commits NEW.md in
// the task's tree, as the agent would), `domains` + `egress` (a stub controller granting
// `qa-token`), and the reviewer: `flagThrows` (the flag reader throws: no gate), `pin` (a refused
// pin's `{ code, reason }`), `call` (the session's answer), `callWaits` (the call returns only once
// its signal aborts), `callCancels` (the call cancels the task first, then waits), `deadlineMs`,
// `engineThrowsAfter` (the host's `engine()` throws from that call on: the run's own call is the
// first, the review's key read the second). An answer `cancel` cancels the task from the card and
// answers `aborted` once the task's signal aborts (code-service.cjs's askApproval on a cancel). The
// sandbox's `{ lock: true }` locks the task's worktree (`git worktree lock`), so its release cannot
// remove it and records it as stuck: a failed workspace release before the review.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

const serverDir = path.resolve(process.argv[2] || '');
process.env.PATH = '/usr/local/bin:/usr/bin:/bin';
process.env.LANG = 'C.UTF-8';
const ID = '00000000-0000-4000-8000-0000000000bb';
crypto.randomUUID = () => ID;
const { createCodeHarness } = require(path.join(serverDir, 'code-harness.cjs'));
const { createPlannerReview } = require(path.join(serverDir, 'code-review.cjs'));
const { createCodeWorkspaces } = require(path.join(serverDir, 'code-workspace.cjs'));
const { createJobs } = require(path.join(serverDir, 'jobs.cjs'));
const { createAcpTransport } = require(path.join(serverDir, 'code-acp.cjs'));

const line = (m) => JSON.stringify(m) + '\n';
const reply = (id, result) => line({ jsonrpc: '2.0', id, result });
const HANDSHAKE = [{ expect: true }, { expect: true }, { send: reply(1, { protocolVersion: 1, agentInfo: { name: 'qa-agent', version: '0.1' } }) },
  { expect: true }, { send: reply(2, { sessionId: 'qa-session' }) }, { expect: true }];
const update = (u) => ({ send: line({ jsonrpc: '2.0', method: 'session/update', params: { update: u } }) });
const PLAN = update({ sessionUpdate: 'plan', entries: [{ content: 'Write NEW.md' }, { content: 'Say done' }] });
// A worktree task's work is what it commits (release removes the tree): the agent commits NEW.md.
const WRITE = [{ send: line({ jsonrpc: '2.0', id: 'w1', method: 'fs/write_text_file', params: { path: 'NEW.md', content: 'written by the QA agent\n' } }) }, { expect: true },
  { commit: true }];
const END = [{ send: reply(3, { stopReason: 'end_turn' }) }, { expectClose: true }];
const WRITES = [...HANDSHAKE, PLAN, ...WRITE, ...END];
const verdict = (v) => ({ ok: true, text: JSON.stringify(v), corrected: false });
const APPROVE = verdict({ verdict: 'approve', summary: 'QA change looks right.', findings: [] });

const SCENARIOS = {
  'approved-and-accepted': { capabilities: ['read_repository', 'edit_file'], answers: ['approve'], call: APPROVE, script: WRITES },
  'changes-requested-on-a-network-task': {
    capabilities: ['read_repository', 'edit_file', 'network'], domains: ['registry.example.invalid'],
    egress: { confirmed: true, report: { available: true, state: 'closed', final: false, partial: false, requests: 0 } },
    answers: ['deny'],
    call: { ok: true, corrected: true, text: JSON.stringify({ verdict: 'request_changes', summary: 'QA needs a test.',
      findings: [{ severity: 'major', file: 'NEW.md', message: 'QA: add a test.' }] }) },
    script: WRITES,
  },
  'review-failed-approve-all': { capabilities: ['read_repository', 'edit_file'], answers: ['approve_all'],
    call: { ok: false, code: 'context_refused', reason: 'QA leak' }, script: WRITES },
  'pin-refused': { capabilities: ['read_repository', 'edit_file'], answers: ['approve'],
    pin: { code: 'external', reason: 'The task pipeline runs only on a local model.' }, script: WRITES },
  'unreadable-verdict-card-aborted': { capabilities: ['read_repository', 'edit_file'], answers: ['aborted'],
    call: { ok: true, text: 'not a verdict', corrected: false }, script: WRITES },
  'nothing-changed-card-timeout': { capabilities: ['read_repository'], answers: ['timeout'], call: APPROVE, script: [...HANDSHAKE, ...END] },
  'review-deadline': { capabilities: ['read_repository', 'edit_file'], answers: ['deny'], callWaits: true, deadlineMs: 50, script: WRITES },
  'cancelled-during-review': { capabilities: ['read_repository', 'edit_file'], callCancels: true, script: WRITES },
  'cancelled-run': { capabilities: ['read_repository'], call: APPROVE, script: [...HANDSHAKE, { cancel: true }, { expectClose: true }] },
  'agent-error': { capabilities: ['read_repository'], call: APPROVE,
    script: [...HANDSHAKE, { send: line({ jsonrpc: '2.0', id: 3, error: { code: -32000, message: 'QA agent failure' } }) }, { expectClose: true }] },
  'engine-throws-at-review': { capabilities: ['read_repository', 'edit_file'], answers: ['approve'], call: APPROVE, engineThrowsAfter: 1, script: WRITES },
  'cancelled-at-the-card': { capabilities: ['read_repository', 'edit_file'], answers: ['cancel'], call: APPROVE, script: WRITES },
  'release-failed': { capabilities: ['read_repository', 'edit_file'], answers: ['approve'], call: APPROVE,
    script: [...HANDSHAKE, PLAN, ...WRITE, { lock: true }, ...END] },
  'flag-reader-throws': { capabilities: ['read_repository', 'edit_file'], flagThrows: true, call: APPROVE, script: WRITES },
};

const gitIn = (cwd, ...a) => execFileSync('git', a, { cwd, stdio: 'ignore', env: { PATH: process.env.PATH, HOME: '/nonexistent', GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null',
  GIT_AUTHOR_NAME: 'QA', GIT_AUTHOR_EMAIL: 'qa@example.invalid', GIT_COMMITTER_NAME: 'QA', GIT_COMMITTER_EMAIL: 'qa@example.invalid',
  GIT_AUTHOR_DATE: '2026-01-01T00:00:00Z', GIT_COMMITTER_DATE: '2026-01-01T00:00:00Z' } });

function repo(base) {
  const r = path.join(base, 'qa-repo');
  fs.mkdirSync(r);
  const git = (...a) => gitIn(r, ...a);
  git('init', '-q', '--template=', '-b', 'main');
  fs.writeFileSync(path.join(r, 'a.txt'), 'qa a\n');
  git('add', '.'); git('commit', '-qm', 'QA first');
  return r;
}

function sandbox(script, tree, onCancel) {
  const lines = [];
  let closed = false;
  return new Promise((ready) => {
    const server = net.createServer((sock) => {
      sock.setEncoding('utf8');
      let buffered = '', waiters = [], closeWaiter = null;
      sock.on('data', (d) => {
        buffered += d;
        let i;
        while ((i = buffered.indexOf('\n')) !== -1) { lines.push(buffered.slice(0, i)); buffered = buffered.slice(i + 1); const w = waiters.shift(); if (w) w(); }
      });
      sock.on('close', () => { closed = true; if (closeWaiter) closeWaiter(); });
      sock.on('error', () => {});
      (async () => {
        let seen = 0;
        for (const step of script) {
          if (step.expect) { if (lines.length <= seen) await new Promise((r) => waiters.push(r)); seen++; }
          else if (step.send !== undefined) { await new Promise((r) => setTimeout(r, 5)); sock.write(step.send.split('<TREE>').join(tree())); }
          else if (step.commit) { gitIn(tree(), 'add', 'NEW.md'); gitIn(tree(), 'commit', '-qm', 'QA change'); }
          else if (step.lock) { gitIn(tree(), 'worktree', 'lock', tree()); }
          else if (step.cancel) { await new Promise((r) => setTimeout(r, 20)); onCancel(); }
          else if (step.expectClose) { if (!closed) await new Promise((r) => { closeWaiter = r; }); }
        }
        server.close();
      })();
    });
    server.listen(0, '127.0.0.1', () => ready({ port: server.address().port, lines }));
  });
}

async function run(sc) {
  const base = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'qa-review-gate-')));
  const dir = path.join(base, 'tenant');
  fs.mkdirSync(dir);
  const jobs = createJobs({ dir, kinds: ['code'], maxJobs: 50, retainMs: 14 * 86400000, now: () => 5000 });
  const workspaces = createCodeWorkspaces({ dir, epoch: 'qa-epoch', now: () => 5000 });
  const asked = [], logs = [], reviewLogs = [], reviewCalls = [];
  const answers = [...(sc.answers || [])];
  let tree = null, harness = null, engineCalls = 0;
  const egress = sc.egress ? {
    endpoint: 'egress:3128',
    available: () => true,
    grant: async () => ({ token: 'qa-token', id: 'qa-grant' }),
    revoke: async () => sc.egress,
    forget: async () => ({ confirmed: true }),
  } : null;
  const waitAbort = (signal) => new Promise((r) => (signal.aborted ? r() : signal.addEventListener('abort', r, { once: true })));
  const roleEngine = {
    pinModel: async (args) => {
      reviewCalls.push({ pin: JSON.parse(JSON.stringify(args)) });
      if (sc.pin) return { ok: false, ...sc.pin };
      return { ok: true, session: { call: async (req) => {
        const { role, state, schemaName, constrain, maxBytes, maxTokens } = req;
        reviewCalls.push({ call: JSON.parse(JSON.stringify({ role, state, schemaName, constrain, maxBytes, maxTokens })) });
        if (sc.callCancels) harness.cancel(ID);
        if (sc.callWaits || sc.callCancels) { await waitAbort(req.signal); return { ok: false, code: 'aborted', reason: 'QA aborted' }; }
        return sc.call;
      } } };
    },
  };
  const review = createPlannerReview({
    enabled: () => { if (sc.flagThrows) throw Error('QA flag reader'); return true; },
    roleEngine, log: (e) => reviewLogs.push(e), ...(sc.deadlineMs ? { deadlineMs: sc.deadlineMs } : {}),
  });
  const box = await sandbox(sc.script, () => tree, () => harness.cancel(ID));
  harness = createCodeHarness({
    jobs, workspaces, egress, now: () => 5000, log: (e) => logs.push(e), review,
    engine: () => {
      if (sc.engineThrowsAfter !== undefined && engineCalls++ >= sc.engineThrowsAfter) throw Error('QA engine');
      return { baseUrl: 'http://qa-engine.invalid:8080/v1', model: 'qa-model', apiKey: 'qa-engine-key', contextTokens: 8192 };
    },
    askApproval: async (request, { signal } = {}) => {
      asked.push(JSON.parse(JSON.stringify(request)));
      const answer = answers.shift() ?? 'deny';
      if (answer !== 'cancel') return answer;
      harness.cancel(ID);
      await waitAbort(signal);
      return 'aborted';
    },
    guard: { enabled: () => false },
  });
  const transport = createAcpTransport({ endpoint: `127.0.0.1:${box.port}` });
  const connect = (args) => { tree = args.cwd; return transport(args); };
  const started = await harness.start({ projectId: 'qa-project', repoPath: repo(base), prompt: 'Write NEW.md for the QA test.', capabilities: sc.capabilities || [],
    domains: sc.domains || [], harness: 'opencode', connect, model: null, sandboxKind: 'sandbox', promptPreparation: 'direct', context: '', tenantId: 'qa-tenant' });
  for (let i = 0; i < 1000 && !['completed', 'failed', 'cancelled'].includes(jobs.get(ID)?.status); i++) await new Promise((r) => setTimeout(r, 10));
  await new Promise((r) => setTimeout(r, 50));
  const norm = (v) => JSON.parse(JSON.stringify(v ?? null).split(tree || '\u0000').join('<TREE>').split(base).join('<BASE>'));
  const rows = fs.readFileSync(path.join(dir, 'jobs', `${ID}.jsonl`), 'utf8');
  const out = { ...sc, started: norm(started), journal: rows.split('\n').filter(Boolean).map((l) => norm(JSON.parse(l))),
    asked: norm(asked), logs: norm(logs), reviewLogs: norm(reviewLogs), reviewCalls: norm(reviewCalls), sandboxLines: box.lines.map((l) => norm(l)) };
  fs.rmSync(base, { recursive: true, force: true });
  return out;
}

(async () => {
  const keep = setInterval(() => {}, 1000);
  const sha = (n) => crypto.createHash('sha256').update(fs.readFileSync(path.join(serverDir, n))).digest('hex');
  const out = {};
  for (const [name, sc] of Object.entries(SCENARIOS)) { process.stderr.write(`scenario ${name}\n`); out[name] = await run(sc); }
  process.stdout.write(JSON.stringify({ generator: 'crates/code-harness/tools/gen-review-gate-fixtures.cjs',
    sources: { 'code-harness.cjs': sha('code-harness.cjs'), 'code-review.cjs': sha('code-review.cjs'), 'jobs.cjs': sha('jobs.cjs'), 'code-workspace.cjs': sha('code-workspace.cjs') },
    scenarios: out }, null, 1) + '\n', () => process.exit(0));
  clearInterval(keep);
})();
