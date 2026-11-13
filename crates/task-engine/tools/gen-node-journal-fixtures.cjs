'use strict';
// Writes tests/fixtures/node-journals.v1.json: synthetic job journals written by noevia-core's own
// jobs.cjs, with the view its get() and the JS task-lifecycle answer for each. The Rust differential
// test (tests/differential.rs) replays the same operations through task-engine and compares.
//
// Usage: node tools/gen-node-journal-fixtures.cjs <noevia-core>/server > tests/fixtures/node-journals.v1.json
// Synthetic data only: fixed ids, a fixed clock, no tenant or model data. deriveLifecycle confirms its
// fold through the dav-parse wasm: put the build pinned by server/dav-parse.lock at server/wasm/ first.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');

const serverDir = path.resolve(process.argv[2] || '');
// The recorded sources; a changed Node file needs a fresh recording (and a reviewed diff).
const SOURCES = {
  'server/jobs.cjs': '3a04aea9e594428d62cb5dc7f29eb937d322c6b24d83c9b2d9b08211366e5457',
  'server/task-lifecycle.cjs': 'f99c9f9387f647fd579fbcd3acc6c6a19531995313cd4cc6894b333633ec5da1',
};
for (const [rel, want] of Object.entries(SOURCES)) {
  const got = crypto.createHash('sha256').update(fs.readFileSync(path.join(serverDir, path.basename(rel)))).digest('hex');
  if (got !== want) { process.stderr.write(`source hash mismatch for ${rel}: ${got}\n`); process.exit(1); }
}
const jobsModule = require(path.join(serverDir, 'jobs.cjs'));
const lifecycle = require(path.join(serverDir, 'task-lifecycle.cjs'));

// Deterministic ids: jobs.cjs calls crypto.randomUUID() for every new job.
let uuidCount = 0;
crypto.randomUUID = () => `00000000-0000-4000-8000-${String(++uuidCount).padStart(12, '0')}`;

const SCENARIOS = {
  'start-complete': [
    { op: 'create', kind: 'code', projectId: 'qa-project', capabilities: ['read', 'write'] },
    { op: 'start' },
    { op: 'progress', stage: 'implementing' },
    { op: 'complete', result: { ok: true, note: 'synthetic' } },
  ],
  'approval-approve-then-complete': [
    { op: 'create', kind: 'code', projectId: 'qa-project', capabilities: ['write', 'write', 'shell'] },
    { op: 'start' },
    { op: 'request', card: { id: 'appr-1', action: 'write', path: 'src/a.txt' } },
    { op: 'decide', approvalId: 'appr-1', decision: 'approve' },
    { op: 'progress', stage: 'verifying' },
    { op: 'complete', result: null },
  ],
  'approval-denied-then-fail': [
    { op: 'create', kind: 'code', capabilities: ['shell'] },
    { op: 'start' },
    { op: 'request', card: { id: 'appr-2', action: 'shell' } },
    { op: 'decide', approvalId: 'appr-2', decision: 'deny' },
    { op: 'fail', error: 'synthetic failure' },
  ],
  'approve-all-and-timeout': [
    { op: 'create', kind: 'chat' },
    { op: 'start' },
    { op: 'request', card: { id: 'appr-3' } },
    { op: 'decide', approvalId: 'appr-3', decision: 'approve_all' },
    { op: 'request', card: { id: 'appr-4' } },
    { op: 'decide', approvalId: 'appr-4', decision: 'timeout' },
    { op: 'cancel' },
  ],
  'cancel-queued': [
    { op: 'create', kind: 'code' },
    { op: 'cancel' },
  ],
  'cancel-running': [
    { op: 'create', kind: 'code' },
    { op: 'start' },
    { op: 'cancel' },
  ],
  'cancel-while-waiting': [
    { op: 'create', kind: 'code' },
    { op: 'start' },
    { op: 'request', card: { id: 'appr-5' } },
    { op: 'cancel' },
  ],
  'cancel-after-finish-is-a-no-op': [
    { op: 'create', kind: 'code' },
    { op: 'start' },
    { op: 'complete', result: { done: 1 } },
    { op: 'cancel' },
  ],
  'failed-at-claim': [
    { op: 'create', kind: 'code' },
    { op: 'fail', error: 'no worktree' },
  ],
  'interrupted-after-restart': [
    { op: 'create', kind: 'code' },
    { op: 'start' },
    { op: 'request', card: { id: 'appr-6' } },
    { op: 'recover' },
  ],
  'completed-from-queued-breaks-lifecycle': [
    { op: 'create', kind: 'code' },
    { op: 'complete', result: 'synthetic' },
  ],
  'write-after-finish-refused': [
    { op: 'create', kind: 'code' },
    { op: 'start' },
    { op: 'complete', result: 1 },
    { op: 'progress', stage: 'implementing' },
  ],
  'start-twice-refused-by-the-engine-only': [
    { op: 'create', kind: 'code' },
    { op: 'start' },
    { op: 'start' },
  ],
  'unknown-type-refused': [
    { op: 'create', kind: 'code' },
    { op: 'append', type: 'not.a.type', data: {} },
  ],
};

const DIVERGENT = new Set(['start-twice-refused-by-the-engine-only']);

function readRows(dir, id) {
  const text = fs.readFileSync(path.join(dir, 'jobs', `${id}.jsonl`), 'utf8');
  return text.split('\n').filter(Boolean).map((line) => JSON.parse(line));
}

function run(name, ops) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), `task-engine-${name}-`));
  let clock = 1000;
  const jobs = jobsModule.createJobs({ dir, now: () => (clock += 10) });
  let id = null;
  const refusals = [];
  for (const [index, step] of ops.entries()) {
    try {
      switch (step.op) {
        case 'create':
          id = jobs.create({ kind: step.kind, projectId: step.projectId ?? null, capabilities: step.capabilities ?? [] });
          break;
        case 'start': jobs.append(id, 'job.started'); break;
        case 'progress': jobs.append(id, 'progress', { stage: step.stage }); break;
        case 'request': jobs.append(id, 'approval.requested', step.card); break;
        case 'decide': jobs.append(id, 'approval.decided', { decision: step.decision }); break;
        case 'complete': jobs.append(id, 'job.completed', { result: step.result }); break;
        case 'fail': jobs.append(id, 'job.failed', { error: step.error }); break;
        case 'cancel': jobs.cancel(id); break;
        case 'interrupt': jobs.append(id, 'job.interrupted', { reason: step.reason }); break;
        case 'recover': jobs.recover(); break;
        case 'append': jobs.append(id, step.type, step.data); break;
        default: throw new Error(`unknown op ${step.op}`);
      }
      refusals.push(null);
    } catch (error) {
      refusals.push(error.status ?? 500);
    }
    if (index === 0 && id === null) throw new Error('first op must create');
  }
  const rows = readRows(dir, id);
  const events = rows.map((r) => ({ ...r }));
  const view = jobs.get(id);
  let lifecycleState = null;
  // deriveLifecycle: the production fold (the JS answer, confirmed by the pinned dav-parse wasm port).
  try { lifecycleState = lifecycle.deriveLifecycle(events); } catch { lifecycleState = null; }
  fs.rmSync(dir, { recursive: true, force: true });
  return {
    name,
    agree: !DIVERGENT.has(name),
    ops,
    refusals,
    journal: rows,
    expect: {
      kind: view.kind,
      projectId: view.projectId,
      parentId: view.parentId,
      capabilities: view.capabilities,
      status: view.status,
      stage: view.stage ?? null,
      pendingApproval: view.pendingApproval ?? null,
      result: view.result ?? null,
      error: view.error ?? null,
      lifecycle: lifecycleState,
      createdAt: view.createdAt,
      updatedAt: view.updatedAt,
    },
  };
}

const scenarios = Object.entries(SCENARIOS).map(([name, ops]) => run(name, ops));
process.stdout.write(`${JSON.stringify({ version: 1, generator: 'tools/gen-node-journal-fixtures.cjs', sources: SOURCES, scenarios }, null, 1)}\n`);
