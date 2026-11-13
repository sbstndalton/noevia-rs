'use strict';
// Writes tests/fixtures/executor-node.v1.json: whole Code task runs by noevia-core's own
// code-harness.cjs `start` (jobs.cjs store, code-workspace.cjs worktree, code-harness-config.cjs
// pin files, code-acp.cjs over a real TCP connection) against a scripted fake sandbox, with a stub
// egress controller: the task's journal file, its workspace record, the egress calls, the approval
// cards, the log lines and every line the sandbox received. tests/executor.rs runs the same script
// against the Rust executor and compares.
//
// Usage: node crates/code-harness/tools/gen-executor-fixtures.cjs <noevia-core>/server > crates/code-harness/tests/fixtures/executor-node.v1.json
// Synthetic data only: a QA repository, fixed ids, a fixed clock, a fake engine address.
//
// Sandbox script steps (the same shape as crates/code-acp's): { expect: true } waits for one line
// from noevia; { send: "<text>" } writes to it; { cancel: true } cancels the task (the harness's
// `cancel`); { expectClose: true } waits for the close. `<TREE>` in a sent line is the task's worktree.
// A scenario may set `engine` (the deployment's endpoint), `prompt`, and `grantNull` (the egress
// controller grants nothing). `engineHosts` is what Node's URL parser makes of engine addresses
// (core `engineHost`: `new URL(engine().baseUrl).hostname`, '' when it throws).

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

const serverDir = path.resolve(process.argv[2] || '');
process.env.PATH = '/usr/local/bin:/usr/bin:/bin';
process.env.LANG = 'C.UTF-8';
crypto.randomUUID = () => '00000000-0000-4000-8000-0000000000bb';
const { createCodeHarness } = require(path.join(serverDir, 'code-harness.cjs'));
const { createCodeWorkspaces } = require(path.join(serverDir, 'code-workspace.cjs'));
const { createJobs } = require(path.join(serverDir, 'jobs.cjs'));
const { createAcpTransport } = require(path.join(serverDir, 'code-acp.cjs'));

const line = (m) => JSON.stringify(m) + '\n';
const reply = (id, result) => line({ jsonrpc: '2.0', id, result });
const HANDSHAKE = [{ expect: true }, { expect: true }, { send: reply(1, { protocolVersion: 1, agentInfo: { name: 'qa-agent', version: '0.1' } }) },
  { expect: true }, { send: reply(2, { sessionId: 'qa-session' }) }, { expect: true }];
const OPTIONS = [{ optionId: 'y', kind: 'allow_once' }, { optionId: 'n', kind: 'reject_once' }];

const SCENARIOS = {
  'completes': {
    capabilities: ['read_repository', 'edit_file', 'execute_command'],
    answers: ['approve'],
    script: [...HANDSHAKE,
      { send: line({ jsonrpc: '2.0', id: 'w1', method: 'fs/write_text_file', params: { path: 'NEW.md', content: 'written by the QA agent\n' } }) },
      { expect: true },
      { send: line({ jsonrpc: '2.0', method: 'session/update', params: { update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'Done.' } } } }) },
      { send: line({ jsonrpc: '2.0', id: 'p1', method: 'session/request_permission', params: { toolCall: { kind: 'execute', title: 'npm test', rawInput: { command: 'npm test' } }, options: OPTIONS } }) },
      { expect: true },
      { send: reply(3, { stopReason: 'end_turn', _meta: { usage: { inputTokens: 7, outputTokens: 3 } } }) },
      { expectClose: true }],
  },
  'agent-error': {
    capabilities: ['read_repository'],
    script: [...HANDSHAKE, { send: line({ jsonrpc: '2.0', id: 3, error: { code: -32000, message: 'QA agent failure' } }) }, { expectClose: true }],
  },
  'sandbox-closes': {
    capabilities: ['read_repository'],
    script: [...HANDSHAKE, { close: true }],
  },
  'network-task': {
    capabilities: ['read_repository', 'network'],
    domains: ['registry.example.invalid'],
    egress: { confirmed: true, report: { available: true, state: 'closed', final: true, partial: false, snapshotId: 'qa-snap', requests: 2 } },
    script: [...HANDSHAKE, { send: reply(3, { stopReason: 'end_turn' }) }, { expectClose: true }],
  },
  'network-agent-error-cleanup-unconfirmed': {
    capabilities: ['read_repository', 'network'],
    domains: ['registry.example.invalid'],
    egress: null,
    egressRevokeFails: true,
    script: [...HANDSHAKE, { send: line({ jsonrpc: '2.0', id: 3, error: { code: -32000, message: 'QA agent failure' } }) }, { expectClose: true }],
  },
  'cancelled': {
    capabilities: ['read_repository'],
    script: [...HANDSHAKE, { cancel: true }, { expectClose: true }],
  },
  'engine-falsy-values': {
    capabilities: ['read_repository', 'network'],
    domains: ['registry.example.invalid'],
    egress: { confirmed: true, report: { available: true, state: 'closed', final: false, partial: false, requests: 0 } },
    engine: { baseUrl: 'http://[FE80::1]:8080/v1', model: '', apiKey: 'qa-engine-key', contextTokens: 0 },
    script: [...HANDSHAKE, { send: reply(3, { stopReason: 'end_turn' }) }, { expectClose: true }],
  },
  'grant-null': {
    capabilities: ['read_repository', 'network'],
    domains: ['registry.example.invalid'],
    egress: { confirmed: true, report: { available: true, state: 'closed', final: true, partial: false } },
    grantNull: true,
    script: [...HANDSHAKE, { send: reply(3, { stopReason: 'end_turn' }) }, { expectClose: true }],
  },
  'blank-prompt-bom': {
    prompt: '\ufeff \u2028',
    script: [],
  },
  'network-cleanup-unconfirmed': {
    capabilities: ['read_repository', 'install_dependency'],
    domains: ['registry.example.invalid'],
    egress: null,
    egressRevokeFails: true,
    script: [...HANDSHAKE, { send: reply(3, { stopReason: 'end_turn' }) }, { expectClose: true }],
  },
};

function repo(base) {
  const r = path.join(base, 'qa-repo');
  fs.mkdirSync(r);
  const git = (...a) => execFileSync('git', a, { cwd: r, stdio: 'ignore', env: { PATH: process.env.PATH, HOME: '/nonexistent', GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null',
    GIT_AUTHOR_NAME: 'QA', GIT_AUTHOR_EMAIL: 'qa@example.invalid', GIT_COMMITTER_NAME: 'QA', GIT_COMMITTER_EMAIL: 'qa@example.invalid',
    GIT_AUTHOR_DATE: '2026-01-01T00:00:00Z', GIT_COMMITTER_DATE: '2026-01-01T00:00:00Z' } });
  git('init', '-q', '--template=', '-b', 'main');
  fs.writeFileSync(path.join(r, 'a.txt'), 'qa a\n');
  git('add', '.'); git('commit', '-qm', 'QA first');
  return r;
}

const ENGINE_HOSTS = ['http://qa-engine.invalid:8080/v1', 'http://[::1]:8080/v1', 'http://[FE80::1]/', 'http://llama\\@evil.invalid/',
  'http:llama:8080/v1', 'http:\\\\llama:8080\\v1', 'http://0x7f.1:8080/', 'http://LLAMA%41:8080/', 'HTTP://Qa-Engine.Invalid./v1',
  ' http://qa-engine.invalid ', 'qa-engine.invalid:8080', 'file:///srv/x', 'http://user:pw@qa-engine.invalid:8080/', 'http://xn--nxasmq6b.invalid/',
  'http://bücher.invalid/', 'http://1.2.3.4.5/', 'http://256.1.1.1/', '', 'not a url'];

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
          else if (step.close) { sock.destroy(); }
          else if (step.cancel) { await new Promise((r) => setTimeout(r, 20)); onCancel(); }
          else if (step.expectClose) { if (!closed) await new Promise((r) => { closeWaiter = r; }); }
        }
        server.close();
      })();
    });
    server.listen(0, '127.0.0.1', () => ready({ port: server.address().port, lines, closed: () => closed }));
  });
}

async function run(name, sc) {
  const base = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'qa-executor-')));
  const dir = path.join(base, 'tenant');
  fs.mkdirSync(dir);
  const jobs = createJobs({ dir, kinds: ['code'], maxJobs: 50, retainMs: 14 * 86400000, now: () => 5000 });
  const workspaces = createCodeWorkspaces({ dir, epoch: 'qa-epoch', now: () => 5000 });
  const asked = [], logs = [], egressCalls = [];
  const answers = [...(sc.answers || [])];
  let tree = null;
  const egress = sc.egress !== undefined || sc.egressRevokeFails ? {
    endpoint: 'egress:3128',
    available: () => true,
    grant: async ({ taskId, domains }) => { egressCalls.push({ grant: { taskId, domains } }); return sc.grantNull ? null : { token: 'qa-token', id: 'qa-grant' }; },
    revoke: async (grant) => { egressCalls.push({ revoke: grant.id }); if (sc.egressRevokeFails) throw Error('QA egress down'); return sc.egress; },
    forget: async (grant, snapshotId) => { egressCalls.push({ forget: [grant.id, snapshotId] }); return { confirmed: true }; },
  } : null;
  let harness = null;
  const box = sc.script.length ? await sandbox(sc.script, () => tree, () => harness.cancel('00000000-0000-4000-8000-0000000000bb')) : { lines: [] };
  harness = createCodeHarness({
    jobs, workspaces, egress, now: () => 5000, log: (e) => logs.push(e),
    engine: () => (sc.engine || { baseUrl: 'http://qa-engine.invalid:8080/v1', model: 'qa-model', apiKey: 'qa-engine-key', contextTokens: 8192 }),
    askApproval: async (request) => { asked.push(JSON.parse(JSON.stringify(request))); return answers.shift() ?? 'deny'; },
    guard: { enabled: () => false },
  });
  const transport = createAcpTransport({ endpoint: `127.0.0.1:${box.port || 9}` });
  const connect = (args) => { tree = args.cwd; return transport(args); };
  let started;
  try {
    started = await harness.start({ projectId: 'qa-project', repoPath: repo(base), prompt: sc.prompt ?? 'Write NEW.md for the QA test.', capabilities: sc.capabilities || [],
      domains: sc.domains || [], harness: 'opencode', connect, model: null, sandboxKind: 'sandbox', promptPreparation: 'direct', context: '', tenantId: 'qa-tenant' });
  } catch (e) { started = { error: String(e.message) }; }
  const id = '00000000-0000-4000-8000-0000000000bb';
  for (let i = 0; i < 1000 && !started.error && !['completed', 'failed', 'cancelled'].includes(jobs.get(id)?.status); i++) await new Promise((r) => setTimeout(r, 10));
  await new Promise((r) => setTimeout(r, 50));
  const norm = (v) => JSON.parse(JSON.stringify(v ?? null).split(tree || '\u0000').join('<TREE>').split(base).join('<BASE>')
    .replace(/"(uid|gid)":\d+/g, '"$1":"<$1>"'));
  const read = (f) => (fs.existsSync(f) ? fs.readFileSync(f, 'utf8') : null);
  const rows = read(path.join(dir, 'jobs', `${id}.jsonl`));
  const journal = rows === null ? null : rows.split('\n').filter(Boolean).map((l) => norm(JSON.parse(l)));
  const rec = read(path.join(dir, 'code-workspaces', `${id}.json`));
  const record = rec === null ? null : norm(JSON.parse(rec));
  const out = { capabilities: sc.capabilities || [], domains: sc.domains || [], answers: sc.answers || [], egress: sc.egressRevokeFails ? 'fails' : (sc.egress ?? null),
    engine: sc.engine ?? null, prompt: sc.prompt ?? null, grantNull: sc.grantNull === true,
    script: sc.script, started: norm(started), journal, record, egressCalls: norm(egressCalls), asked: norm(asked), logs: norm(logs), sandboxLines: box.lines.map((l) => norm(l)) };
  fs.rmSync(base, { recursive: true, force: true });
  return out;
}

(async () => {
  const keep = setInterval(() => {}, 1000);
  const sha = (n) => crypto.createHash('sha256').update(fs.readFileSync(path.join(serverDir, n))).digest('hex');
  const out = {};
  for (const [name, sc] of Object.entries(SCENARIOS)) { process.stderr.write(`scenario ${name}\n`); out[name] = await run(name, sc); }
  process.stdout.write(JSON.stringify({ generator: 'crates/code-harness/tools/gen-executor-fixtures.cjs',
    sources: { 'code-harness.cjs': sha('code-harness.cjs'), 'code-acp.cjs': sha('code-acp.cjs'), 'jobs.cjs': sha('jobs.cjs'), 'code-workspace.cjs': sha('code-workspace.cjs'), 'code-harness-config.cjs': sha('code-harness-config.cjs') },
    env: { PATH: process.env.PATH, LANG: process.env.LANG },
    engineHosts: ENGINE_HOSTS.map((baseUrl) => [baseUrl, (() => { try { return new URL(baseUrl).hostname; } catch { return ''; } })()]),
    scenarios: out }, null, 1) + '\n', () => process.exit(0));
  clearInterval(keep);
})();
