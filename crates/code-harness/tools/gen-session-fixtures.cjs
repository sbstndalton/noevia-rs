'use strict';
// Writes tests/fixtures/session-node.v1.json: what noevia-core's own code-harness.cjs session
// (`createSession`: the permission gate, the agent's file API, the session-update record) answers
// to scripted ACP calls from a stub agent, and every journal call, approval card and log line it
// makes while the prompt runs. tests/session.rs replays each script through the Rust session.
//
// Usage: node crates/code-harness/tools/gen-session-fixtures.cjs <noevia-core>/server > crates/code-harness/tests/fixtures/session-node.v1.json
// Synthetic data only: a throwaway QA repository, fixed ids, a clock the script sets.
//
// Script steps: { call: 'requestPermission'|'readTextFile'|'writeTextFile', params } (an answer),
// { update } (a session update), { clock } (the session's clock), { fs: { write|symlink|mkdir } }
// (the agent touching its tree directly). `<TREE>` in any string is the task's worktree.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

const serverDir = path.resolve(process.argv[2] || '');
crypto.randomUUID = () => '00000000-0000-4000-8000-0000000000aa';
const { createCodeHarness } = require(path.join(serverDir, 'code-harness.cjs'));
const { createCodeWorkspaces } = require(path.join(serverDir, 'code-workspace.cjs'));
const { createJobs } = require(path.join(serverDir, 'jobs.cjs'));

const OPTIONS = [{ optionId: 'y', kind: 'allow_once', name: 'Allow' }, { optionId: 'a', kind: 'allow_always', name: 'Always' },
  { optionId: 'n', kind: 'reject_once', name: 'Reject' }];
const perm = (toolCall) => ({ call: 'requestPermission', params: { toolCall, options: OPTIONS } });
const edit = (file) => perm({ kind: 'edit', title: 'Edit ' + file, locations: [{ path: file }], rawInput: { path: file } });
const exec = (command, extra = {}) => perm({ kind: 'execute', title: command, rawInput: { command, ...extra } });

const SCENARIOS = {
  'permissions': {
    capabilities: ['read_repository', 'edit_file', 'execute_command'],
    answers: ['approve', 'approve_all', 'deny', 'timeout', 'aborted', 'approve', 'approve_all', 'approve'],
    script: [
      edit('<TREE>/a.txt'),
      edit('/etc/passwd'),
      exec('npm test'),
      exec('npm run lint'),
      exec('npm run build'),
      exec('rm -rf build'),
      exec('git push origin HEAD'),
      exec('curl https://example.invalid/x'),
      perm({ kind: 'read', title: 'Read a.txt', locations: [{ path: '<TREE>/a.txt' }] }),
      perm({ kind: 'think', title: 'Thinking' }),
      exec('sh -c "echo $HOME"'),
      perm({ kind: 'delete', title: 'Delete', locations: [{ path: '<TREE>/a.txt' }] }),
      perm({ kind: 'delete', title: 'Delete again', locations: [{ path: '<TREE>/a.txt' }] }),
      { call: 'requestPermission', params: { toolCall: { kind: 'execute', rawInput: { command: 'ls' } }, options: [] } },
      { call: 'requestPermission', params: {} },
      { call: 'requestPermission' },
      perm({ kind: 'edit', title: 'diff', locations: [{ path: '<TREE>/b.txt' }], rawInput: { path: '<TREE>/b.txt' },
        content: [{ type: 'diff', path: '<TREE>/b.txt', oldText: 'old', newText: 'x'.repeat(5000) }] }),
    ],
  },
  'announced-calls': {
    capabilities: ['read_repository', 'edit_file', 'execute_command'],
    answers: ['approve', 'approve', 'approve', 'approve'],
    script: [
      { update: { sessionUpdate: 'tool_call', toolCallId: 'c1', title: 'bash', kind: 'other', status: 'pending', rawInput: { command: 'rm -rf build', description: 'clean' } } },
      perm({ toolCallId: 'c1' }),
      { update: { sessionUpdate: 'tool_call', toolCallId: 'c2', kind: 'other', rawInput: { command: 'echo hi' } } },
      perm({ toolCallId: 'c2', rawInput: { command: 'git push' } }),
      { update: { sessionUpdate: 'tool_call_update', toolCallId: 'c2', status: 'completed', rawOutput: { exitCode: 0 }, content: [{ type: 'content', text: 'ok' }] } },
      perm({ toolCallId: 'c2' }),
      { update: { sessionUpdate: 'tool_call', toolCallId: 'c3', kind: 'read', title: 'cat', rawInput: { command: 'cat a.txt' } } },
      perm({ toolCallId: 'c3', kind: null }),
      { update: { sessionUpdate: 'tool_call_update', toolCallId: 'c3', status: 'failed', _meta: { exit_code: 2 }, content: [{ text: 'no such file' }, { text: 'second' }, 5] } },
      { update: { sessionUpdate: 'tool_call_update', toolCallId: 'c4', status: 'in_progress', title: 'later' } },
    ],
  },
  'files': {
    capabilities: ['read_repository', 'edit_file'],
    script: [
      { call: 'readTextFile', params: { path: '<TREE>/a.txt' } },
      { call: 'readTextFile', params: { path: 'a.txt' } },
      { call: 'readTextFile', params: { path: 'lines.txt', line: 2, limit: 2 } },
      { call: 'readTextFile', params: { path: 'lines.txt', line: 0 } },
      { call: 'readTextFile', params: { path: 'lines.txt', line: '3' } },
      { call: 'readTextFile', params: { path: 'lines.txt', limit: 1 } },
      { call: 'readTextFile', params: { path: '/etc/hostname' } },
      { call: 'readTextFile', params: { path: '../outside.txt' } },
      { call: 'readTextFile', params: {} },
      { call: 'writeTextFile', params: { path: 'new/deep/ok.txt', content: 'fine' } },
      { call: 'writeTextFile', params: { path: '<TREE>/a.txt', content: 'é😀' } },
      { call: 'writeTextFile', params: { path: 'num.txt', content: 42 } },
      { call: 'writeTextFile', params: { path: 'nul.txt' } },
      { call: 'writeTextFile', params: { path: '/tmp/qa-escape.txt', content: 'x' } },
      { fs: { symlink: 'link-out', to: '/etc' } },
      { call: 'writeTextFile', params: { path: 'link-out/x', content: 'x' } },
      { call: 'readTextFile', params: { path: 'link-out/hostname' } },
      { fs: { write: 'lines.txt', text: 'one\ntwo\nthree\nfour\n' } },
      { call: 'readTextFile', params: { path: 'lines.txt', line: 2, limit: 2 } },
    ],
  },
  'updates-and-output': {
    capabilities: ['read_repository'],
    script: [
      { clock: 1000 },
      { update: { sessionUpdate: 'agent_thought_chunk', content: { type: 'text', text: 'hidden reasoning' } } },
      { clock: 1100 },
      { update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'Hello ' } } },
      { clock: 1700 },
      { update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'world, ' + 'é'.repeat(600) } } },
      { update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'pair \ud83d' } } },
      { update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: '\ude00 joined' } } },
      { update: { sessionUpdate: 'tool_call', toolCallId: 't1', title: 'ls', kind: 'execute', rawInput: { command: 'ls' } } },
      { update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'after the tool' } } },
      { update: { sessionUpdate: 'agent_message_chunk', content: { type: 'image', data: 'x' } } },
      { update: { sessionUpdate: 'plan', entries: [{ content: 'step one' }, { content: 'step two' }, { notContent: 1 }, ...Array.from({ length: 25 }, (_, i) => ({ content: `s${i}` }))] } },
      { update: { sessionUpdate: 'usage_update', used: 8012, size: 24576, cost: { amount: 0, currency: 'USD' } } },
      { update: { sessionUpdate: 'usage_update', usage: { inputTokens: 10, outputTokens: 5 } } },
      { update: { sessionUpdate: 'available_commands_update', availableCommands: [] } },
      { update: { type: 'agent_message_chunk', content: { type: 'text', text: ' via type' } } },
      { update: { sessionUpdate: 'unknown_kind' } },
      { clock: 1800 },
      { update: { sessionUpdate: 'agent_thought_chunk' } },
      { update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'x'.repeat(40000) } } },
      { update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'after the cap' } } },
    ],
  },
  'guard-blocks': {
    guarded: true,
    capabilities: ['read_repository', 'edit_file', 'execute_command'],
    answers: ['approve', 'approve'],
    script: [
      perm({ kind: 'execute', rawInput: { command: 42 } }),
      { call: 'writeTextFile', params: { path: 'a.txt' } },
      { call: 'readTextFile', params: { path: 5 } },
      exec('npm test'),
      perm({ kind: 'execute', rawInput: {} }),
      perm({ kind: 'execute', rawInput: { cmd: ['echo', 1] } }),
      exec('npm test'),
      { call: 'readTextFile', params: { path: 'a.txt' } },
    ],
  },
};

const T = (s, tree) => JSON.parse(JSON.stringify(s).split('<TREE>').join(tree));
const norm = (v, tree, dir) => JSON.parse(JSON.stringify(v ?? null).split(tree).join('<TREE>').split(dir).join('<DIR>'));

function repo(base) {
  const r = path.join(base, 'qa-repo');
  fs.mkdirSync(r);
  const git = (...a) => execFileSync('git', a, { cwd: r, stdio: 'ignore', env: { PATH: process.env.PATH, HOME: '/nonexistent', GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null',
    GIT_AUTHOR_NAME: 'QA', GIT_AUTHOR_EMAIL: 'qa@example.invalid', GIT_COMMITTER_NAME: 'QA', GIT_COMMITTER_EMAIL: 'qa@example.invalid',
    GIT_AUTHOR_DATE: '2026-01-01T00:00:00Z', GIT_COMMITTER_DATE: '2026-01-01T00:00:00Z' } });
  git('init', '-q', '--template=', '-b', 'main');
  fs.writeFileSync(path.join(r, 'a.txt'), 'qa a\n');
  fs.writeFileSync(path.join(r, 'lines.txt'), 'l1\nl2\nl3\nl4\nl5\n');
  git('add', '.'); git('commit', '-qm', 'QA first');
  return r;
}

async function run(name, sc) {
  const base = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'qa-session-')));
  const dir = path.join(base, 'tenant');
  fs.mkdirSync(dir);
  const realJobs = createJobs({ dir });
  const calls = [];
  let recording = false;
  // Every ctx call the work makes, recorded while the prompt runs.
  const jobs = { ...realJobs, run: (id, work) => realJobs.run(id, (ctx) => {
    const wrap = (fn, f) => (...args) => { if (recording) calls.push({ fn, args: JSON.parse(JSON.stringify(args)) }); return f(...args); };
    return work({ ...ctx, event: wrap('event', ctx.event), progress: wrap('progress', ctx.progress), checkpoint: wrap('checkpoint', ctx.checkpoint),
      artifact: wrap('artifact', ctx.artifact), uncertain: wrap('uncertain', ctx.uncertain) });
  }) };
  const workspaces = createCodeWorkspaces({ dir, epoch: 'qa-epoch' });
  const asked = [], logs = [];
  const answers = [...(sc.answers || [])];
  let clock = 0;
  const harness = createCodeHarness({
    jobs, workspaces, now: () => clock, log: (e) => { if (recording) logs.push(e); },
    engine: () => ({ baseUrl: 'http://qa-engine.invalid/v1', model: 'qa-model', apiKey: null, contextTokens: 8192 }),
    askApproval: async (request, { signal } = {}) => { asked.push(JSON.parse(JSON.stringify(request))); if (signal?.aborted) return 'aborted'; return answers.shift() ?? 'deny'; },
    guard: { enabled: () => sc.guarded === true },
  });
  const results = [];
  let tree = null;
  const started = await harness.start({
    repoPath: repo(base), prompt: 'QA task', capabilities: sc.capabilities || [], domains: sc.domains || [], harness: 'opencode', sandboxKind: 'spawn',
    connect: async ({ handlers, cwd }) => ({ agent: { name: 'qa-agent', version: '1' }, prompt: async () => {
      tree = cwd;
      recording = true;
      for (const step of sc.script) {
        if (step.clock !== undefined) { clock = step.clock; results.push(null); continue; }
        if (step.update !== undefined) { handlers.sessionUpdate(T(step.update, cwd)); results.push(null); continue; }
        if (step.fs) {
          const f = step.fs;
          if (f.write) fs.writeFileSync(path.join(cwd, f.write), f.text);
          if (f.symlink) fs.symlinkSync(f.to, path.join(cwd, f.symlink));
          results.push(null); continue;
        }
        try {
          const params = step.params === undefined ? undefined : T(step.params, cwd);
          const out = await handlers[step.call](params);
          results.push({ ok: out === undefined ? null : JSON.parse(JSON.stringify(out)) });
        } catch (e) {
          results.push({ error: { code: e.code ?? null, message: String(e.message), data: e.data === undefined ? null : JSON.parse(JSON.stringify(e.data)) } });
        }
      }
      recording = false;
      return { stopReason: 'end_turn' };
    } }),
  });
  for (let i = 0; i < 400 && !['completed', 'failed', 'cancelled'].includes(realJobs.get(started.taskId)?.status); i++) await new Promise((r) => setTimeout(r, 5));
  const job = realJobs.get(started.taskId);
  const out = { capabilities: sc.capabilities || [], domains: sc.domains || [], guarded: sc.guarded === true, answers: sc.answers || [], script: sc.script,
    results: norm(results, tree, dir), calls: norm(calls, tree, dir), asked: norm(asked, tree, dir), logs: norm(logs, tree, dir),
    job: norm({ status: job.status, error: job.error, result: job.result }, tree, dir) };
  fs.rmSync(base, { recursive: true, force: true });
  return out;
}

(async () => {
  const keep = setInterval(() => {}, 1000);
  const sha = (n) => crypto.createHash('sha256').update(fs.readFileSync(path.join(serverDir, n))).digest('hex');
  const out = {};
  for (const [name, sc] of Object.entries(SCENARIOS)) out[name] = await run(name, sc);
  process.stdout.write(JSON.stringify({ generator: 'crates/code-harness/tools/gen-session-fixtures.cjs',
    sources: { 'code-harness.cjs': sha('code-harness.cjs'), 'code-actions.cjs': sha('code-actions.cjs'), 'code-tool-schemas.cjs': sha('code-tool-schemas.cjs'), 'code-meta.cjs': sha('code-meta.cjs') },
    scenarios: out }, null, 1) + '\n');
  clearInterval(keep);
})();
