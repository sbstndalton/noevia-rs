'use strict';
// Writes tests/fixtures/service-node.v1.json: core's own code-service.cjs (`createCodeService`)
// answering Code mode's requests as routes/code.cjs sends them (status and body, its error mapping
// included), over the real task stack (jobs.cjs, code-workspace.cjs, code-harness.cjs, code-acp.cjs
// over TCP) against a scripted fake sandbox, with stub egress and maintenance gate: every answer,
// the task's journal, the log lines and every line the sandbox received. Also parseRepos,
// parseUser and defaultHarnesses on a table of inputs. tests/service.rs replays it on the Rust
// service.
//
// Usage: node crates/code-harness/tools/gen-service-fixtures.cjs <noevia-core>/server > crates/code-harness/tests/fixtures/service-node.v1.json
// Synthetic data only: a QA repository, counted ids, a fixed clock, a fake engine address.
//
// Operations: { start: body } POST /code; { decide: [task, decision, approval] } POST approve
// (`<T1>`/`<A1>` = the first task / approval id); { cancel: task } POST cancel; { facts: true };
// { waitCard: task } waits until the task has a card waiting; { waitEnd: task } until it ended;
// { sleep: ms }. Sandbox steps as in gen-executor-fixtures.cjs.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const net = require('node:net');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

const serverDir = path.resolve(process.argv[2] || '');
process.env.PATH = '/usr/local/bin:/usr/bin:/bin';
process.env.LANG = 'C.UTF-8';
let counter = 0;
const idOf = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
crypto.randomUUID = () => idOf(++counter);
const { createCodeService, parseRepos, parseUser, defaultHarnesses } = require(path.join(serverDir, 'code-service.cjs'));
const { createAcpTransport } = require(path.join(serverDir, 'code-acp.cjs'));

const line = (m) => JSON.stringify(m) + '\n';
const reply = (id, result) => line({ jsonrpc: '2.0', id, result });
const HANDSHAKE = [{ expect: true }, { expect: true }, { send: reply(1, { protocolVersion: 1, agentInfo: { name: 'qa-agent', version: '0.1' } }) },
  { expect: true }, { send: reply(2, { sessionId: 'qa-session' }) }, { expect: true }];
const OPTIONS = [{ optionId: 'y', kind: 'allow_once' }, { optionId: 'n', kind: 'reject_once' }];
const ASK = (id) => ({ send: line({ jsonrpc: '2.0', id, method: 'session/request_permission', params: { toolCall: { kind: 'execute', title: 'npm test', rawInput: { command: 'npm test' } }, options: OPTIONS } }) });
const END = { send: reply(3, { stopReason: 'end_turn' }) };
const BASE = { repository: 'qa-repo', prompt: 'QA: run the tests.' };

const SCENARIOS = {
  'refusals': {
    ops: [
      { facts: true },
      { start: { ...BASE, repository: 'elsewhere' } },
      { start: { ...BASE, repository: 7 } },
      { start: { ...BASE, prompt: '  ﻿ ' } },
      { start: { ...BASE, prompt: 'x'.repeat(8001) } },
      { start: { ...BASE, prompt: '\u{1F600}'.repeat(4001) } },
      { start: { ...BASE, harness: 'codex' } },
      { start: { ...BASE, promptPreparation: 'planner' } },
      { start: { ...BASE, promptPreparation: 'local' } },
      { start: { ...BASE, promptPreparation: 'frontier' } },
      { decide: ['00000000-0000-4000-8000-0000000000ff', 'approve', 'x'] },
      { cancel: '00000000-0000-4000-8000-0000000000ff' },
    ],
    script: [],
  },
  'approve-flow': {
    egress: true,
    ops: [
      { start: { ...BASE, capabilities: ['network', 'edit_file', 'execute_command', 'browser', 7, 'git_push', 'network'],
        domains: [' Registry.Example.ORG ', 'bad', 'a.b', 'x.example.com1', 7, null, 'ok-1.example.io', '.io'], model: 'qa-model-b', harness: 'opencode' } },
      { waitCard: '<T1>' },
      { facts: true },
      { start: BASE },
      { decide: ['<T1>', 'approve', 'not-the-card'] },
      { decide: ['<T1>', 'maybe', '<A1>'] },
      { decide: ['<T1>', 'approve', ''] },
      { decide: ['<T1>', 'approve', '<A1>'] },
      { waitEnd: '<T1>' },
      { decide: ['<T1>', 'approve', '<A1>'] },
      { cancel: '<T1>' },
      { facts: true },
    ],
    script: [...HANDSHAKE, ASK('p1'), { expect: true }, END, { expectClose: true }],
  },
  'timeout': {
    timeoutMs: 300,
    ops: [{ start: BASE }, { waitCard: '<T1>' }, { sleep: 600 }, { decide: ['<T1>', 'approve', '<A1>'] }, { waitEnd: '<T1>' }],
    script: [...HANDSHAKE, ASK('p1'), { expect: true }, END, { expectClose: true }],
  },
  'cancel-while-waiting': {
    ops: [{ start: BASE }, { waitCard: '<T1>' }, { cancel: '<T1>' }, { waitEnd: '<T1>' },
      { decide: ['<T1>', 'approve', '<A1>'] }, { cancel: '<T1>' }, { facts: true }],
    script: [...HANDSHAKE, ASK('p1'), { expectClose: true }],
  },
  'paused': {
    inference: 'paused',
    ops: [{ start: BASE }],
    script: [],
  },
  'lease-refused': {
    inference: 'refuses',
    project: { id: 'qa-project', sharedContext: { code: true, chat: 'yes' } },
    ops: [{ start: BASE }, { waitEnd: '<T1>' }, { cancel: '<T1>' }],
    script: [],
  },
  'shared-context': {
    project: { id: 'qa-project', sharedContext: { code: true } },
    // No egress: the network capabilities are not granted at all.
    ops: [{ start: { ...BASE, promptPreparation: 'direct', capabilities: ['network', 'install_dependency', 'edit_file'], domains: ['registry.example.org'] } },
      { waitEnd: '<T1>' }],
    script: [...HANDSHAKE, END, { expectClose: true }],
  },
};

function git(r, ...a) {
  execFileSync('git', a, { cwd: r, stdio: 'ignore', env: { PATH: process.env.PATH, HOME: '/nonexistent', GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null',
    GIT_AUTHOR_NAME: 'QA', GIT_AUTHOR_EMAIL: 'qa@example.invalid', GIT_COMMITTER_NAME: 'QA', GIT_COMMITTER_EMAIL: 'qa@example.invalid',
    GIT_AUTHOR_DATE: '2026-01-01T00:00:00Z', GIT_COMMITTER_DATE: '2026-01-01T00:00:00Z' } });
}

function repo(base, name = 'qa-repo') {
  const r = path.join(base, name);
  fs.mkdirSync(r);
  git(r, 'init', '-q', '--template=', '-b', 'main');
  fs.writeFileSync(path.join(r, 'a.txt'), 'qa a\n');
  git(r, 'add', '.'); git(r, 'commit', '-qm', 'QA first');
  return r;
}

function sandbox(script) {
  const lines = [];
  return new Promise((ready) => {
    const server = net.createServer((sock) => {
      sock.setEncoding('utf8');
      let buffered = '', waiters = [], closeWaiter = null, closed = false;
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
          else if (step.send !== undefined) { await new Promise((r) => setTimeout(r, 5)); sock.write(step.send); }
          else if (step.expectClose) { if (!closed) await new Promise((r) => { closeWaiter = r; }); }
        }
        server.close();
      })();
    });
    server.listen(0, '127.0.0.1', () => ready({ port: server.address().port, lines, server }));
  });
}

async function run(name, sc) {
  counter = 0;
  const base = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'qa-service-')));
  const dir = path.join(base, 'tenant');
  fs.mkdirSync(dir);
  const logs = [], egressCalls = [];
  const box = sc.script.length ? await sandbox(sc.script) : { lines: [], port: 9 };
  const egress = sc.egress ? {
    endpoint: 'egress:3128',
    available: () => true,
    grant: async ({ taskId, domains }) => { egressCalls.push({ grant: { taskId, domains } }); return { token: 'qa-token', id: 'qa-grant' }; },
    revoke: async (grant) => { egressCalls.push({ revoke: grant.id }); return { confirmed: true, report: { available: true, state: 'closed', final: false, partial: false } }; },
    forget: async () => ({ confirmed: true }),
  } : null;
  const inference = sc.inference ? {
    paused: () => sc.inference === 'paused',
    enter: () => { if (sc.inference === 'refuses') throw Object.assign(Error('QA gate held'), { status: 503 }); return () => {}; },
  } : null;
  const service = createCodeService({
    repos: [{ id: 'qa-repo', path: repo(base) }], connect: createAcpTransport({ endpoint: `127.0.0.1:${box.port}` }), egress,
    engine: () => ({ baseUrl: 'http://qa-engine.invalid:8080/v1', model: 'qa-model', apiKey: 'qa-engine-key', contextTokens: 8192 }),
    now: () => 5000, log: (e) => logs.push(e), timeoutMs: sc.timeoutMs || 60000, sandboxKind: 'sandbox',
    harnesses: defaultHarnesses({ CODE_HARNESS_NAME: 'opencode', CODE_HARNESS_VERSION: '0.0.0-qa' }), treeRoot: null, harnessUser: null,
    sharedContext: (ws, project) => (project.sharedContext?.code === true ? 'QA shared context: the goal is a green test run.' : ''), inference,
  });
  const workspace = { dir, userId: 'qa-user' };
  const project = sc.project || { id: 'qa-project' };
  // routes/code.cjs: the answer and its error mapping.
  const route = async (status, fn) => {
    try { return { status, body: await fn() }; }
    catch (error) { return { status: error.status || 500, body: { error: error.publicMessage || (error.status ? error.message : 'Code request failed') } }; }
  };
  const ids = { tasks: [], approvals: [] };
  const sub = (v) => String(v).replace(/<T(\d)>/g, (_, n) => ids.tasks[n - 1] || 'none').replace(/<A(\d)>/g, (_, n) => ids.approvals[n - 1] || 'none');
  const answers = [];
  for (const op of sc.ops) {
    if (op.start) {
      const a = await route(202, () => service.start(workspace, project, op.start));
      if (a.body.taskId) ids.tasks.push(a.body.taskId);
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
      for (let i = 0; i < 1000 && !card; i++) { card = service.facts(workspace).approvals[t] || null; if (!card) await new Promise((r) => setTimeout(r, 10)); }
      if (card && !ids.approvals.includes(card.id)) ids.approvals.push(card.id);
      answers.push({ waited: card ? 'card' : 'none' });
    } else if (op.waitEnd) {
      const t = sub(op.waitEnd);
      const ended = () => { const f = path.join(dir, 'jobs', `${t}.jsonl`); return fs.existsSync(f) && /"type":"job\.(completed|failed|cancelled|interrupted)"/.test(fs.readFileSync(f, 'utf8')); };
      for (let i = 0; i < 1000 && !ended(); i++) await new Promise((r) => setTimeout(r, 10));
      await new Promise((r) => setTimeout(r, 100));
      answers.push({ waited: ended() ? 'ended' : 'running' });
    } else if (op.sleep) {
      await new Promise((r) => setTimeout(r, op.sleep));
      answers.push({ slept: op.sleep });
    }
  }
  await new Promise((r) => setTimeout(r, 100));
  const norm = (v) => JSON.parse(JSON.stringify(v ?? null).split(base).join('<BASE>'));
  const journals = {};
  for (const t of ids.tasks) {
    const f = path.join(dir, 'jobs', `${t}.jsonl`);
    journals[t] = fs.existsSync(f) ? fs.readFileSync(f, 'utf8').split('\n').filter(Boolean).map((l) => norm(JSON.parse(l))) : null;
  }
  if (box.server) box.server.close();
  const out = { ops: sc.ops, script: sc.script, egress: !!sc.egress, inference: sc.inference || null, timeoutMs: sc.timeoutMs || 60000,
    project, answers: norm(answers), journals, logs: norm(logs), egressCalls: norm(egressCalls), sandboxLines: box.lines.map((l) => norm(l)) };
  fs.rmSync(base, { recursive: true, force: true });
  return out;
}

function tables() {
  const base = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'qa-repos-')));
  const a = repo(base, 'qa-a'), b = repo(base, 'qa-b');
  fs.mkdirSync(path.join(base, 'not-a-repo'));
  fs.symlinkSync(a, path.join(base, 'link-a'));
  const raws = [
    '', ' , ,', `qa|${a}`, ` qa | ${a} , other|${b}`, `qa|${a},qa|${b}`, `qa|relative/path`, `|${a}`, `qa|`, `qa|${base}/missing`,
    `qa|${base}/not-a-repo`, `qa|${base}/link-a`, `qa|${a}|extra`, `qa | ${a}`, `qa|${a}/`, `a|${a},b|${b},c|${a}`,
  ];
  const repos = raws.map((raw) => [raw.split(base).join('<BASE>'), JSON.parse(JSON.stringify(parseRepos(raw)).split(base).join('<BASE>'))]);
  fs.rmSync(base, { recursive: true, force: true });
  const users = ['', '1000:1000', ' 1000:1000 ', '1000', '1000:', ':1000', '12345678:1', '1234567:7654321', '0:0', 'a:1', '1:1:1', '١:1', '+1:1', '1 :1'].map((raw) => [raw, parseUser(raw)]);
  const harnesses = [{}, { CODE_HARNESS_NAME: '' }, { CODE_HARNESS_NAME: '  ' }, { CODE_HARNESS_NAME: ' pi ' }, { CODE_HARNESS_NAME: 'claude-code', CODE_HARNESS_VERSION: '2.1' },
    { CODE_HARNESS_NAME: 'deepseek', CODE_HARNESS_VERSION: '' }, { CODE_HARNESS_NAME: 'my-harness' }, { CODE_HARNESS_NAME: 'codex' }, { CODE_HARNESS_NAME: 'qwen-code' }]
    .map((env) => [env, defaultHarnesses(env)]);
  return { repos, users, harnesses };
}

(async () => {
  const keep = setInterval(() => {}, 1000);
  const sha = (n) => crypto.createHash('sha256').update(fs.readFileSync(path.join(serverDir, n))).digest('hex');
  const out = {};
  for (const [name, sc] of Object.entries(SCENARIOS)) { process.stderr.write(`scenario ${name}\n`); out[name] = await run(name, sc); }
  clearInterval(keep);
  process.stdout.write(JSON.stringify({ generator: 'crates/code-harness/tools/gen-service-fixtures.cjs',
    sources: { 'code-service.cjs': sha('code-service.cjs'), 'code-harness.cjs': sha('code-harness.cjs'), 'jobs.cjs': sha('jobs.cjs'), 'routes/code.cjs': sha('routes/code.cjs'), 'shared-context.cjs': sha('shared-context.cjs') },
    env: { PATH: process.env.PATH, LANG: process.env.LANG }, tables: tables(), scenarios: out }, null, 1) + '\n', () => process.exit(0));
})();
