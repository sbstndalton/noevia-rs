'use strict';
// Writes tests/fixtures/task-store/node.v1.json: what noevia-core's own server/jobs.cjs answers, and
// the exact bytes it leaves in the jobs/ directory, for scripted synthetic operations. The Rust test
// (tests/task_store.rs) replays every operation through server_projects::task_store in a fresh
// directory and compares each answer and every file, byte for byte.
//
// Usage (from a noevia-core checkout with server/wasm/dav-parse.wasm at its pinned sha256):
//   node crates/server-projects/tools/gen-task-store-fixtures.cjs <noevia-core>/server \
//     > crates/server-projects/tests/fixtures/task-store/node.v1.json
// Synthetic data only: fixed ids, a clock the script sets, no tenant or model data.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');

const serverDir = path.resolve(process.argv[2] || '');
const SOURCES = ['jobs.cjs', 'code-plan.cjs', 'code-review-verdict.cjs', 'task-lifecycle.cjs'];

// jobs.cjs calls crypto.randomUUID() for every created job; the script names the next one.
let nextUuid = null;
crypto.randomUUID = () => { const id = nextUuid; nextUuid = null; if (!id) throw Error('no uuid set'); return id; };
const { createJobs } = require(path.join(serverDir, 'jobs.cjs'));

const id = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
const hex = (buf) => Buffer.from(buf).toString('hex');
const line = (row) => JSON.stringify(row) + '\n';
const row = (job, seq, type, at, data) => ({ job, seq, type, at, data });
function chained(rows) {
  let previous = null;
  return rows.map((r) => {
    const out = { ...r, previous };
    out.hash = crypto.createHash('sha256').update(JSON.stringify(out)).digest('hex');
    previous = out.hash;
    return out;
  });
}

// Each store is one createJobs() over the scenario's directory.
const STORES = {
  code: { kinds: ['code'], maxJobs: 3, retainMs: 1000 },
  any: {},
  durable: { kinds: ['code'], durable: true, maxJobs: 50, retainMs: 14 * 86400000 },
  durableAll: { durable: true },
};

const long = 'é'.repeat(150) + '😀'.repeat(200); // 300 + 800 bytes
const A = id(1), B = id(2), C = id(3), D = id(4), E = id(5), F = id(6), G = id(7);
const created = (job, at, kind, extra = {}) => row(job, 1, 'job.created', at, { kind, projectId: 'qa-project', parentId: null, capabilities: [], ...extra });

const SCENARIOS = {
  'lifecycle-code': [
    { op: 'clock', t: 1000 },
    { op: 'create', store: 'code', uuid: A, kind: 'code', projectId: 'qa-project', capabilities: ['read', 'edit', 'edit'] },
    { op: 'clock', t: 1001 },
    { op: 'append', store: 'code', id: A, type: 'job.started' },
    { op: 'append', store: 'code', id: A, type: 'step.started', data: { id: 'harness.config', title: 'Pin the harness configuration' } },
    { op: 'append', store: 'code', id: A, type: 'step.completed', data: { id: 'harness.config', b: 1, '2': 'two', a: [1, 0.1, -0, 1e21, 1e-7, 123456789012345680000], '1': null } },
    { op: 'append', store: 'code', id: A, type: 'progress', data: { stage: 'implementing' } },
    { op: 'append', store: 'code', id: A, type: 'approval.requested', data: { taskId: A, action: 'execute', command: 'npm test', reason: 'quote " backslash \\ tab \t nl \n ctl \u0001 \u001f del \u007f ls   ps   lone \ud800 pair 😀' } },
    { op: 'append', store: 'code', id: A, type: 'approval.decided', data: { decision: 'approve', action: 'execute' } },
    { op: 'append', store: 'code', id: A, type: 'tool.started', data: { id: 't1', name: 'bash' } },
    { op: 'append', store: 'code', id: A, type: 'tool.completed', data: { id: 't1', failed: false } },
    { op: 'append', store: 'code', id: A, type: 'tool.uncertain', data: { id: 't2' } },
    { op: 'append', store: 'code', id: A, type: 'artifact.created', data: { name: 'diff', kind: 'patch' } },
    { op: 'append', store: 'code', id: A, type: 'checkpoint.created', data: { branch: 'noevia/qa', task: 'QA task' } },
    { op: 'clock', t: 1500 },
    { op: 'append', store: 'code', id: A, type: 'job.completed', data: { result: { ok: true, files: 2 } } },
    { op: 'append', store: 'code', id: A, type: 'progress', data: { stage: 'late' } },
    { op: 'append', store: 'code', id: A, type: 'artifact.created', data: { name: 'late' } },
    { op: 'cancel', store: 'code', id: A },
    { op: 'get', store: 'code', id: A },
  ],
  'refusals': [
    { op: 'clock', t: 2000 },
    { op: 'append', store: 'code', id: 'not-a-job', type: 'progress', data: {} },
    { op: 'append', store: 'code', id: A, type: 'progress', data: {} },
    { op: 'append', store: 'code', id: A, type: 'no.such.type', data: {} },
    { op: 'append', store: 'code', id: A, type: 'task.stage', data: { to: 'planning' } },
    { op: 'append', store: 'code', id: A, type: 'task.revision', data: { n: 1 } },
    { op: 'get', store: 'code', id: 'NOT-A-JOB' },
    { op: 'get', store: 'code', id: A },
    { op: 'cancel', store: 'code', id: A },
    { op: 'create', store: 'code', uuid: B, kind: '' },
    { op: 'create', store: 'code', uuid: B, kind: 'code', parentId: A },
    { op: 'create', store: 'code', uuid: B, kind: 'code', parentId: 'bad' },
    { op: 'create', store: 'code', uuid: B, kind: 'code', capabilities: ['read', 'edit'] },
    { op: 'create', store: 'code', uuid: C, kind: 'code', parentId: B, capabilities: ['read'] },
    { op: 'create', store: 'code', uuid: D, kind: 'code', parentId: B, capabilities: ['read', 'execute', 'install', 'execute'] },
    { op: 'append', store: 'code', id: B, type: 'job.created', data: { kind: 'code', capabilities: ['execute'] } },
  ],
  'cancel-and-late-artifact': [
    { op: 'clock', t: 3000 },
    { op: 'create', store: 'any', uuid: A, kind: 'code' },
    { op: 'append', store: 'any', id: A, type: 'job.started' },
    { op: 'clock', t: 3001 },
    { op: 'cancel', store: 'any', id: A },
    { op: 'append', store: 'any', id: A, type: 'artifact.created', data: { name: 'partial' } },
    { op: 'append', store: 'any', id: A, type: 'progress', data: { stage: 'x' } },
    { op: 'create', store: 'any', uuid: B, kind: 'browser' },
    { op: 'append', store: 'any', id: B, type: 'job.failed', data: { error: 'synthetic' } },
    { op: 'append', store: 'any', id: B, type: 'artifact.created', data: { name: 'late' } },
  ],
  'assistant-output': [
    { op: 'clock', t: 4000 },
    { op: 'create', store: 'any', uuid: A, kind: 'code' },
    { op: 'append', store: 'any', id: A, type: 'assistant.output', data: { text: 'short' } },
    { op: 'append', store: 'any', id: A, type: 'assistant.output', data: { text: long } },
    { op: 'append', store: 'any', id: A, type: 'assistant.output', data: { text: 'x'.repeat(1023) + 'é' } },
    { op: 'append', store: 'any', id: A, type: 'assistant.output', data: { text: 'lone \ud83d tail', truncated: true } },
    { op: 'append', store: 'any', id: A, type: 'assistant.output', data: { text: 42, truncated: 'yes' } },
    { op: 'append', store: 'any', id: A, type: 'assistant.output' },
    { op: 'repeat', count: 58, step: { op: 'append', store: 'any', id: A, type: 'assistant.output', data: { text: 'n' } } },
    { op: 'append', store: 'any', id: A, type: 'assistant.output', data: { text: 'over the limit' } },
    { op: 'get', store: 'any', id: A },
    { op: 'create', store: 'any', uuid: B, kind: 'browser' },
    { op: 'append', store: 'any', id: B, type: 'assistant.output', data: { text: 'not code' } },
  ],
  'plans-and-reviews': [
    { op: 'clock', t: 5000 },
    { op: 'create', store: 'any', uuid: A, kind: 'code' },
    { op: 'append', store: 'any', id: A, type: 'plan.proposed', data: { question: 'dropped', subQuestions: ['one', long, 7, null, { a: 1 }, [1, 2], true, ...Array.from({ length: 20 }, (_, i) => `s${i}`)], extra: 1 } },
    { op: 'append', store: 'any', id: A, type: 'plan.edited', data: { subQuestions: ['only'], truncated: true } },
    { op: 'append', store: 'any', id: A, type: 'plan.edited', data: { subQuestions: 'not a list' } },
    { op: 'append', store: 'any', id: A, type: 'plan.skipped', data: { question: 'q', subQuestions: ['x'], truncated: true } },
    { op: 'append', store: 'any', id: A, type: 'review.requested', data: { baseSha: 'a'.repeat(40), headSha: 'b'.repeat(40), files: 3 } },
    { op: 'append', store: 'any', id: A, type: 'review.completed', data: { baseSha: 'a'.repeat(40), headSha: 'b'.repeat(40), verdict: 'request_changes', summary: 'Fix the QA test.', findings: [{ severity: 'major', file: 'src/qa.rs', message: 'QA finding' }], corrected: false } },
    { op: 'append', store: 'any', id: A, type: 'review.completed', data: { verdict: 'approve', grant: 'everything' } },
    { op: 'append', store: 'any', id: A, type: 'review.failed', data: { baseSha: 'a'.repeat(40), headSha: 'b'.repeat(40), code: 'stale', reason: 'The verdict was for an earlier revision.' } },
    { op: 'get', store: 'any', id: A },
    { op: 'create', store: 'any', uuid: B, kind: 'research' },
    { op: 'append', store: 'any', id: B, type: 'plan.proposed', data: { question: 'kept', subQuestions: [long, 7], other: { z: 1, '0': 2 } } },
    { op: 'append', store: 'any', id: B, type: 'plan.skipped', data: { question: 'kept too' } },
    { op: 'append', store: 'any', id: B, type: 'review.requested', data: {} },
  ],
  'torn-tail': [
    { op: 'clock', t: 6000 },
    { op: 'seed', name: `${A}.jsonl`, text: line(created(A, 10, 'code')) + line(row(A, 2, 'job.started', 11, {})) + '{"job":"' + A + '","seq":3,"type":"progr' },
    { op: 'get', store: 'code', id: A },
    { op: 'append', store: 'code', id: A, type: 'progress', data: { stage: 'after the cut' } },
    { op: 'seed', name: `${B}.jsonl`, text: '{"job":"' + B + '","seq":1' },
    { op: 'get', store: 'code', id: B },
    { op: 'append', store: 'code', id: B, type: 'job.started' },
    { op: 'seed', name: `${C}.jsonl`, text: line(created(C, 10, 'code')) + '{"torn":\n' + line(row(C, 3, 'job.started', 12, {})) },
    { op: 'get', store: 'code', id: C },
    { op: 'append', store: 'code', id: C, type: 'progress', data: {} },
    { op: 'seed', name: `${D}.jsonl`, text: '\n\n' + line(created(D, 10, 'code')).replace('\n', '\r\n') + '\n' + line(row(D, 2, 'job.started', 11, { note: 'crlf above' })) },
    { op: 'get', store: 'code', id: D },
    { op: 'append', store: 'code', id: D, type: 'progress', data: { stage: 'crlf' } },
    { op: 'seed', name: `${E}.jsonl`, hex: hex(Buffer.concat([Buffer.from(line(created(E, 10, 'code')).slice(0, -2)), Buffer.from(',"bad":"\xff\xfe\xc3"}\n', 'latin1')])) },
    { op: 'get', store: 'code', id: E },
    { op: 'append', store: 'code', id: E, type: 'job.started' },
    { op: 'seed', name: `${F}.jsonl`, text: '' },
    { op: 'get', store: 'code', id: F },
    { op: 'append', store: 'code', id: F, type: 'job.started' },
  ],
  'hash-chain': [
    { op: 'clock', t: 7000 },
    { op: 'create', store: 'durable', uuid: A, kind: 'code', projectId: 'qa-project', capabilities: ['read'] },
    { op: 'append', store: 'durable', id: A, type: 'job.started' },
    { op: 'append', store: 'durable', id: A, type: 'approval.requested', data: { action: 'edit', path: 'qa.txt', lone: '\udc00' } },
    { op: 'clock', t: 7001 },
    // A store that is not durable still chains a journal whose first row has a hash.
    { op: 'append', store: 'code', id: A, type: 'approval.decided', data: { decision: 'deny' } },
    { op: 'get', store: 'code', id: A },
    { op: 'seed', name: `${B}.jsonl`, text: chained([created(B, 10, 'code'), row(B, 2, 'job.started', 11, {})]).map(line).join('') + '{"job":"' + B },
    { op: 'get', store: 'code', id: B },
    { op: 'append', store: 'code', id: B, type: 'progress', data: {} },
    { op: 'seed', name: `${C}.jsonl`, text: chained([created(C, 10, 'code'), row(C, 2, 'job.started', 11, { n: 1 })]).map(line).join('').replace('"n":1', '"n":2') },
    { op: 'get', store: 'code', id: C },
    { op: 'append', store: 'code', id: C, type: 'progress', data: {} },
    { op: 'seed', name: `${D}.jsonl`, text: chained([created(D, 10, 'code'), row(D, 3, 'job.started', 11, {})]).map(line).join('') },
    { op: 'get', store: 'code', id: D },
    { op: 'seed', name: `${E}.jsonl`, text: line(chained([created(E, 10, 'code')])[0]) + line(row(E, 2, 'job.started', 11, {})) },
    { op: 'get', store: 'code', id: E },
    { op: 'append', store: 'code', id: E, type: 'progress', data: {} },
    { op: 'seed', name: `${F}.jsonl`, text: line(created(F, 10, 'code')) + line({ ...chained([row(F, 2, 'job.started', 11, {})])[0], previous: null }) },
    { op: 'get', store: 'code', id: F },
    // A hash that is falsy does not chain, and `previous` after it is null.
    { op: 'seed', name: `${G}.jsonl`, text: line({ ...created(G, 10, 'code'), hash: '' }) + line({ ...row(G, 2, 'job.started', 11, {}), hash: 0 }) },
    { op: 'get', store: 'code', id: G },
    { op: 'append', store: 'code', id: G, type: 'progress', data: {} },
  ],
  'durable-store': [
    { op: 'clock', t: 8000 },
    // Plain journals of the store's kind fail closed under a durable store; other kinds do not.
    { op: 'seed', name: `${A}.jsonl`, text: line(created(A, 10, 'code')) },
    { op: 'get', store: 'durable', id: A },
    { op: 'append', store: 'durable', id: A, type: 'job.started' },
    { op: 'seed', name: `${B}.jsonl`, text: line(created(B, 10, 'chat')) + '{"torn' },
    { op: 'get', store: 'durable', id: B },
    { op: 'append', store: 'durable', id: B, type: 'job.started' },
    { op: 'seed', name: `${C}.jsonl`, text: '{"torn' },
    { op: 'get', store: 'durable', id: C },
    { op: 'get', store: 'durableAll', id: C },
    { op: 'seed', name: `${D}.jsonl`, text: line(created(D, 10, 'chat')) },
    { op: 'get', store: 'durableAll', id: D },
    { op: 'create', store: 'durableAll', uuid: E, kind: 'chat' },
    { op: 'append', store: 'durableAll', id: E, type: 'job.completed', data: { result: 'done' } },
  ],
  'recover': [
    { op: 'clock', t: 9000 },
    { op: 'create', store: 'code', uuid: A, kind: 'code' },
    { op: 'create', store: 'code', uuid: B, kind: 'code' },
    { op: 'append', store: 'code', id: B, type: 'job.started' },
    { op: 'create', store: 'code', uuid: C, kind: 'code' },
    { op: 'append', store: 'code', id: C, type: 'job.started' },
    { op: 'append', store: 'code', id: C, type: 'approval.requested', data: { action: 'execute' } },
    { op: 'create', store: 'code', uuid: D, kind: 'code' },
    { op: 'append', store: 'code', id: D, type: 'job.failed', data: { error: 'done already' } },
    { op: 'create', store: 'any', uuid: E, kind: 'browser' },
    { op: 'seed', name: `${F}.jsonl`, text: line(created(F, 10, 'code')) + 'not json\n' + line(row(F, 3, 'job.started', 12, {})) },
    { op: 'seed', name: 'notes.jsonl', text: 'not a job\n' },
    { op: 'seed', name: `${G}.json`, text: '{}' },
    { op: 'clock', t: 9100 },
    { op: 'recover', store: 'code' },
    { op: 'recover', store: 'code' },
    { op: 'recover', store: 'any' },
  ],
  'prune': [
    { op: 'clock', t: 10000 },
    { op: 'create', store: 'any', uuid: A, kind: 'code' },
    { op: 'append', store: 'any', id: A, type: 'job.completed', data: {} },
    { op: 'clock', t: 10100 },
    { op: 'create', store: 'any', uuid: B, kind: 'code' },
    { op: 'append', store: 'any', id: B, type: 'job.cancelled', data: {} },
    { op: 'clock', t: 10200 },
    { op: 'create', store: 'any', uuid: C, kind: 'code' },
    { op: 'append', store: 'any', id: C, type: 'job.failed', data: { error: 'x' } },
    { op: 'clock', t: 10300 },
    { op: 'create', store: 'any', uuid: D, kind: 'code' },
    { op: 'append', store: 'any', id: D, type: 'job.interrupted', data: { reason: 'y' } },
    { op: 'clock', t: 10400 },
    { op: 'create', store: 'any', uuid: E, kind: 'chat' },
    { op: 'append', store: 'any', id: E, type: 'job.completed', data: {} },
    { op: 'create', store: 'any', uuid: F, kind: 'code' },
    { op: 'seed', name: `${G}.jsonl`, text: 'broken\n' + line(created(G, 1, 'code')) },
    // maxJobs 3 of kind code: the oldest finished code job goes; the chat job and the running one stay.
    { op: 'prune', store: 'code' },
    { op: 'clock', t: 11250 },
    // retainMs 1000: finished more than a second ago.
    { op: 'prune', store: 'code' },
    { op: 'clock', t: 20000 },
    { op: 'create', store: 'code', uuid: id(8), kind: 'code' },
  ],
  // Node's own behaviour where the Rust store differs on purpose (task_store.rs, "Fixed on purpose").
  'torn-differences': [
    { op: 'clock', t: 12000 },
    // 1. A last row that lost only its newline: jobs.cjs counts it, then cuts it off.
    { op: 'seed', name: `${A}.jsonl`, text: line(created(A, 10, 'code')) + JSON.stringify(row(A, 2, 'job.started', 11, {})) },
    { op: 'get', store: 'code', id: A },
    { op: 'append', store: 'code', id: A, type: 'progress', data: { stage: 'next' } },
    // Its cache still holds the row it cut; a new process reads the file.
    { op: 'get', store: 'code', id: A },
    { op: 'restart' },
    { op: 'get', store: 'code', id: A },
    { op: 'seed', name: `${B}.jsonl`, text: (() => { const r = chained([created(B, 10, 'code'), row(B, 2, 'job.started', 11, {})]); return line(r[0]) + JSON.stringify(r[1]); })() },
    { op: 'get', store: 'code', id: B },
    { op: 'append', store: 'code', id: B, type: 'progress', data: {} },
    { op: 'restart' },
    { op: 'get', store: 'code', id: B },
    // 2. A last line that does not parse but ends in a newline: jobs.cjs writes after it.
    { op: 'seed', name: `${C}.jsonl`, text: line(created(C, 10, 'code')) + 'not json\n' },
    { op: 'get', store: 'code', id: C },
    { op: 'append', store: 'code', id: C, type: 'job.started' },
    { op: 'restart' },
    { op: 'get', store: 'code', id: C },
  ],
};

// The jobs/ directory as {name: bytes}; null when there is none.
function snapshot(dir) {
  const root = path.join(dir, 'jobs');
  let names = [];
  try { names = fs.readdirSync(root).sort(); } catch { return null; }
  return Object.fromEntries(names.map((n) => [n, fs.readFileSync(path.join(root, n))]));
}
// What changed since the last step: each new or changed file, as the bytes it gained (`appended`) or all its
// bytes (`whole`), as text, or null with `hex` when they are not UTF-8; and the names removed. The Rust test checks the whole directory after every step against the running state.
const fatal = new TextDecoder('utf-8', { fatal: true });
function changes(before, after) {
  const out = { written: {}, removed: [] };
  for (const [n, bytes] of Object.entries(after || {})) {
    const old = before?.[n];
    if (old && Buffer.compare(old, bytes) === 0) continue;
    // A plain append is recorded as what it added.
    const grown = old && bytes.length > old.length && Buffer.compare(bytes.subarray(0, old.length), old) === 0;
    const part = grown ? bytes.subarray(old.length) : bytes;
    let text = null;
    try { text = fatal.decode(part); } catch { text = null; }
    out.written[n] = { [grown ? 'appended' : 'whole']: text !== null ? text : null, ...(text === null ? { hex: hex(part) } : {}) };
  }
  for (const n of Object.keys(before || {})) if (!after || !(n in after)) out.removed.push(n);
  return out;
}
const outcome = (fn) => {
  try { return { ok: fn() }; } catch (e) { return { error: { status: e.status ?? null, message: String(e.message) } }; }
};
// What get() says, reduced to what the Rust store's own checks read (the full view is task_journals'
// own differential).
const brief = (job) => job && { id: job.id, kind: job.kind, status: job.status, updatedAt: job.updatedAt };

function runScenario(steps) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-task-store-'));
  let clock = 0;
  const stores = {};
  const store = (name) => (stores[name] ||= createJobs({ dir, now: () => clock, ...STORES[name] }));
  const out = [];
  let files = null;
  const one = (s) => {
    let result = null;
    switch (s.op) {
      case 'clock': clock = s.t; break;
      // A new process: every store is made again, with nothing cached.
      case 'restart': for (const k of Object.keys(stores)) delete stores[k]; break;
      case 'seed':
        fs.mkdirSync(path.join(dir, 'jobs'), { recursive: true });
        fs.writeFileSync(path.join(dir, 'jobs', s.name), s.hex !== undefined ? Buffer.from(s.hex, 'hex') : s.text);
        break;
      case 'create': nextUuid = s.uuid; result = outcome(() => store(s.store).create({ kind: s.kind, projectId: s.projectId, parentId: s.parentId, capabilities: s.capabilities })); nextUuid = null; break;
      case 'append': result = outcome(() => (s.data === undefined ? store(s.store).append(s.id, s.type) : store(s.store).append(s.id, s.type, s.data))); break;
      case 'get': result = outcome(() => brief(store(s.store).get(s.id))); break;
      case 'cancel': result = outcome(() => brief(store(s.store).cancel(s.id))); break;
      case 'recover': result = outcome(() => store(s.store).recover()); break;
      case 'prune': result = outcome(() => (store(s.store).prune(), null)); break;
      default: throw Error(`unknown op ${s.op}`);
    }
    // Step data travels as JSON text, so the Rust side parses exactly what JSON.stringify wrote.
    const { data, ...rest } = s;
    const now = snapshot(dir);
    out.push({ ...rest, ...(data !== undefined ? { data: JSON.stringify(data) } : {}), ...(result ? { result } : {}), files: changes(files, now) });
    files = now;
  };
  for (const s of steps) {
    if (s.op === 'repeat') for (let i = 0; i < s.count; i++) one(s.step);
    else one(s);
  }
  fs.rmSync(dir, { recursive: true, force: true });
  return out;
}

const sources = Object.fromEntries(SOURCES.map((n) => [n, crypto.createHash('sha256').update(fs.readFileSync(path.join(serverDir, n))).digest('hex')]));
const scenarios = Object.fromEntries(Object.entries(SCENARIOS).map(([name, steps]) => [name, runScenario(steps)]));
process.stdout.write(JSON.stringify({ generator: 'crates/server-projects/tools/gen-task-store-fixtures.cjs', sources, stores: STORES, scenarios }, null, 1) + '\n');
