'use strict';
// Writes tests/fixtures/node.v1.json: transcripts of noevia-core's own server/code-acp.cjs
// `connectAcp` talking to a scripted fake sandbox (its `connectFn` seam): every line Node wrote,
// every handler call it made and what it logged, and how connect and prompt ended. The Rust test
// (tests/replay.rs) plays the same fake sandbox over a real TCP socket against the Rust client
// and compares.
//
// Usage: node crates/code-acp/tools/gen-fixtures.cjs <noevia-core>/server > crates/code-acp/tests/fixtures/node.v1.json
// Synthetic data only.

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { EventEmitter } = require('node:events');

const serverDir = path.resolve(process.argv[2] || '');
// agentEnv reads these two from the process: pinned, so the start line is the same everywhere.
process.env.PATH = '/usr/local/bin:/usr/bin:/bin';
process.env.LANG = 'C.UTF-8';
const { connectAcp } = require(path.join(serverDir, 'code-acp.cjs'));

const line = (m) => JSON.stringify(m) + '\n';
const reply = (id, result) => line({ jsonrpc: '2.0', id, result });

// A scenario: connect options, scripted handler answers, and the fake sandbox's script:
//   { expect: true }        wait for Node's next line and record it
//   { send: "<chunk>" }     write a chunk to Node
//   { prompt: "<text>" }    call prompt() on the connection (after connect resolved)
//   { abort: true }         abort the task's signal
//   { close: true }         the sandbox closes the connection
//   { expectClose: true }   wait until Node destroys the connection
const longName = 'qa-agent-' + 'x'.repeat(100);
const SCENARIOS = {
  happy: {
    options: { cwd: '/qa/tree', home: '/qa/home', env: { QA_EXTRA: 'yes' }, permission: { edit: 'ask', bash: 'ask', webfetch: 'ask' },
      harness: 'opencode', model: 'qa-model', proxy: { url: 'http://task:qa-token@egress:3128', noProxy: 'engine' } },
    answers: {
      readTextFile: [{ ok: { content: 'qa file\n' } }],
      writeTextFile: [{ error: { code: -32602, message: 'Outside this task’s workspace', data: { violation: 'path', tool: 'write_file' } } },
        { error: { message: '' } }, { ok: null }],
      requestPermission: [{ ok: { outcome: 'selected', optionId: 'allow-once' } }],
    },
    script: [
      { expect: true }, { expect: true },
      { send: reply(1, { protocolVersion: '1', agentInfo: { name: longName, version: '0.0.1-qa-' + 'v'.repeat(50) } }) },
      { expect: true },
      { send: reply(2, { sessionId: 'qa-session-1' }) },
      { prompt: 'Fix the QA test.' },
      { expect: true },
      { send: line({ jsonrpc: '2.0', id: 'r1', method: 'fs/read_text_file', params: { sessionId: 'qa-session-1', path: '/qa/tree/a.txt' } }) },
      { expect: true },
      { send: line({ jsonrpc: '2.0', id: 7, method: 'fs/write_text_file', params: { path: '../escape', content: 'x' } }) },
      { expect: true },
      { send: line({ jsonrpc: '2.0', id: 8, method: 'fs/write_text_file', params: { path: 'a', content: 'x' } }) },
      { expect: true },
      { send: line({ jsonrpc: '2.0', id: 9, method: 'fs/write_text_file' }) },
      { expect: true },
      // Two messages in one chunk, one split across two chunks, blank and CRLF lines, junk.
      { send: line({ jsonrpc: '2.0', method: 'session/update', params: { sessionId: 's', update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'hé \ud83d lone' } } } })
        + line({ jsonrpc: '2.0', method: 'session/update', params: { sessionUpdate: 'plan', entries: [] } }) },
      { send: '{"jsonrpc":"2.0","method":"session/upd' },
      { send: 'ate","params":{"update":{"sessionUpdate":"usage_update","used":1,"size":2}}}\r\n\n  \n' },
      { send: 'this is not json\n' },
      { send: line({ jsonrpc: '2.0', method: 'unknown/notification', params: {} }) },
      { send: line({ jsonrpc: '2.0', id: 999, result: { stray: true } }) },
      { send: line({ jsonrpc: '2.0', id: 'u1', method: 'terminal/create', params: {} }) },
      { expect: true },
      { send: line({ jsonrpc: '2.0', id: 'p1', method: 'session/request_permission', params: { toolCall: { toolCallId: 't1', title: 'npm test' }, options: [{ optionId: 'allow-once', kind: 'allow_once' }] } }) },
      { expect: true },
      { send: line({ jsonrpc: '2.0', id: null, method: 'session/update', params: { update: { sessionUpdate: 'tool_call', toolCallId: 't1' } } }) },
      { send: reply(3, { stopReason: 'end_turn', _meta: { usage: { inputTokens: 5 } } }) },
      { expectClose: true },
    ],
  },
  'initialize-error': {
    options: { cwd: '/qa/tree', harness: 'opencode' },
    script: [
      { expect: true }, { expect: true },
      { send: line({ jsonrpc: '2.0', id: 1, error: { code: -32000, message: 'QA refusal' } }) },
      { expectClose: true },
    ],
  },
  'no-session': {
    options: { cwd: '/qa/tree', harness: 'opencode' },
    script: [
      { expect: true }, { expect: true },
      { send: reply(1, { protocolVersion: 1, serverInfo: { name: 'qa', version: 3 } }) },
      { expect: true },
      { send: reply(2, {}) },
      { expectClose: true },
    ],
  },
  'cancel-during-prompt': {
    options: { cwd: '/qa/tree', harness: 'opencode' },
    script: [
      { expect: true }, { expect: true },
      { send: reply(1, { protocolVersion: null, _meta: { agentInfo: { name: 'qa' } } }) },
      { expect: true },
      { send: reply(2, { sessionId: 'qa-session-2' }) },
      { prompt: 'Long QA task' },
      { expect: true },
      { abort: true },
      { expect: true },
      { expectClose: true },
    ],
  },
  'sandbox-closes': {
    options: { cwd: '/qa/tree', harness: 'opencode' },
    script: [
      { expect: true }, { expect: true },
      { send: reply(1, {}) },
      { expect: true },
      { send: reply(2, { sessionId: 'qa-session-3' }) },
      { prompt: 'QA' },
      { expect: true },
      { close: true },
    ],
  },
  'prompt-error': {
    options: { cwd: '/qa/tree', harness: 'opencode', model: null },
    script: [
      { expect: true }, { expect: true },
      { send: reply(1, { protocolVersion: -1 }) },
      { expect: true },
      { send: reply(2, { sessionId: 'qa-session-4' }) },
      { prompt: 'QA' },
      { expect: true },
      { send: line({ jsonrpc: '2.0', id: 3, error: { code: 'odd', message: '' } }) },
      { expectClose: true },
    ],
  },
  'claude-code-settings': {
    home: { '.claude/settings.json': JSON.stringify({ permissions: { ask: ['Bash'] }, env: { QA: '1' } }) },
    options: { cwd: '/qa/tree', harness: 'claude-code', permission: { edit: 'ask' } },
    script: [
      { expect: true }, { expect: true },
      { send: reply(1, { protocolVersion: 1 }) },
      { expect: true },
      { send: reply(2, { sessionId: 'qa-session-5' }) },
    ],
  },
  'claude-code-missing-settings': {
    home: {},
    options: { cwd: '/qa/tree', harness: 'claude-code' },
    script: [],
  },
  // Keys whose value is undefined keep their place until agentEnv deletes them.
  'env-order': {
    options: { cwd: '/qa/tree', harness: 'opencode', env: { QA: '1', HOME: '', TMPDIR: '', '7': 'index key' } },
    script: [
      { expect: true }, { expect: true },
      { send: reply(1, { protocolVersion: [[2]] }) },
      { expect: true },
      { send: reply(2, { sessionId: 42 }) },
      { prompt: 'QA' },
      { expect: true },
      // A float id, non-object lines, then a reply carrying both an error and a result.
      { send: line({ jsonrpc: '2.0', id: 7.5, method: 'fs/read_text_file', params: { path: 'a' } }) },
      { expect: true },
      { send: '5\n"x"\n[]\n{}\n' },
      { send: line({ jsonrpc: '2.0', id: 3, error: { code: 1, message: 'both' }, result: { ignored: true } }) },
      { expectClose: true },
    ],
    answers: { readTextFile: [{ ok: { content: 'float id' } }] },
  },
  'spawn-exit': {
    transport: 'spawn',
    options: { cwd: '/qa/tree', harness: 'opencode', command: 'qa-agent', args: ['--acp'] },
    script: [
      { expect: true },
      { send: reply(1, {}) },
      { expect: true },
      { send: reply(2, { sessionId: 'qa-spawned' }) },
      { prompt: 'QA' },
      { expect: true },
      { exit: 3 },
    ],
  },
  'handshake-timeout': {
    options: { cwd: '/qa/tree', harness: 'opencode', handshakeMs: 100 },
    script: [{ expect: true }, { expect: true }, { expectClose: true }],
  },
};

function errorOf(e) {
  return { message: String(e?.message), publicMessage: e?.publicMessage ?? null, status: e?.status ?? null,
    code: e?.code === undefined ? null : e.code };
}

async function run(name, sc) {
  const transcript = { lines: [], handlers: [], logs: [], connect: null, prompt: null, connectedTo: null, closed: false };
  let home = null;
  if (sc.home) {
    home = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-acp-home-'));
    for (const [rel, body] of Object.entries(sc.home)) {
      fs.mkdirSync(path.dirname(path.join(home, rel)), { recursive: true });
      fs.writeFileSync(path.join(home, rel), body);
    }
  }
  const socket = new EventEmitter();
  let buffered = '', waiters = [], destroyed = false, closeWaiter = null;
  socket.setEncoding = () => {};
  socket.write = (text) => {
    buffered += text;
    let i;
    while ((i = buffered.indexOf('\n')) !== -1) {
      const l = buffered.slice(0, i); buffered = buffered.slice(i + 1);
      transcript.lines.push(l);
      const w = waiters.shift(); if (w) w();
    }
    return true;
  };
  socket.destroy = () => { if (destroyed) return; destroyed = true; transcript.closed = true; if (closeWaiter) closeWaiter(); setImmediate(() => socket.emit('close')); };
  const connectFn = (port, host) => { transcript.connectedTo = [host, port]; setImmediate(() => socket.emit('connect')); return socket; };
  // The spawn transport's fake child: its stdin is the socket's write side, its stdout emits.
  const child = new EventEmitter();
  child.pid = 2147483646; // no such process: Node's group kill fails and falls back to child.kill
  child.kill = () => { transcript.closed = true; if (closeWaiter) closeWaiter(); return true; };
  child.stdin = { write: (t) => socket.write(t) };
  child.stdout = new EventEmitter(); child.stdout.setEncoding = () => {};
  child.stderr = new EventEmitter(); child.stderr.setEncoding = () => {};
  const spawnFn = (command, args, opts) => { transcript.spawned = { command, args, cwd: opts.cwd, env: opts.env }; return child; };
  const spawned = sc.transport === 'spawn';
  const answers = JSON.parse(JSON.stringify(sc.answers || {}));
  const answer = (handler, params) => {
    transcript.handlers.push({ handler, params: params === undefined ? null : params });
    const next = (answers[handler] || []).shift();
    if (!next) throw Object.assign(Error('no QA answer'), { code: -32603 });
    if (next.error) throw Object.assign(Error(next.error.message), next.error.code !== undefined ? { code: next.error.code } : {}, next.error.data ? { data: next.error.data } : {});
    return next.ok;
  };
  const handlers = {
    requestPermission: async (p) => answer('requestPermission', p),
    readTextFile: async (p) => answer('readTextFile', p),
    writeTextFile: async (p) => answer('writeTextFile', p),
    sessionUpdate: (u) => { transcript.handlers.push({ handler: 'sessionUpdate', params: u === undefined ? null : u }); },
  };
  const controller = new AbortController();
  const { handshakeMs, ...options } = sc.options;
  const connecting = connectAcp({ ...(spawned ? {} : { endpoint: 'qa-sandbox:9000' }), home, ...options, handlers, signal: controller.signal,
    onLog: (t) => transcript.logs.push(t), connectFn, spawnFn, ...(handshakeMs ? { handshakeMs } : {}) })
    .then((c) => { transcript.connect = { ok: { sessionId: c.sessionId, agent: c.agent } }; return c; },
      (e) => { transcript.connect = { error: errorOf(e) }; return null; });
  let seen = 0, prompting = null;
  for (const step of sc.script) {
    if (process.env.QA_TRACE) process.stderr.write(`  step ${JSON.stringify(step).slice(0, 60)} lines=${transcript.lines.length}\n`);
    if (step.expect) {
      if (transcript.lines.length <= seen) await new Promise((r) => waiters.push(r));
      seen++;
    } else if (step.send !== undefined) {
      await new Promise((r) => setImmediate(r));
      (spawned ? child.stdout : socket).emit('data', step.send);
      await new Promise((r) => setImmediate(r));
    } else if (step.prompt !== undefined) {
      const conn = await connecting;
      prompting = conn.prompt(step.prompt).then((r) => { transcript.prompt = { ok: r === undefined ? null : r }; },
        (e) => { transcript.prompt = { error: errorOf(e) }; });
    } else if (step.abort) {
      controller.abort();
    } else if (step.exit !== undefined) {
      child.emit('exit', step.exit, null);
    } else if (step.close) {
      socket.emit('close');
    } else if (step.expectClose) {
      if (!destroyed) await new Promise((r) => { closeWaiter = r; });
    }
  }
  await connecting;
  if (prompting) await prompting;
  await new Promise((r) => setTimeout(r, 20));
  if (home) fs.rmSync(home, { recursive: true, force: true });
  // The home is a temporary directory: its path is normalised in the start line.
  if (home) transcript.lines = transcript.lines.map((l) => l.split(home).join('<HOME>'));
  return { ...sc, home: sc.home || null, transcript };
}

(async () => {
  // code-acp.cjs unrefs its handshake timer; this keeps the process alive while it runs.
  const keepAlive = setInterval(() => {}, 1000);
  const sha = crypto.createHash('sha256').update(fs.readFileSync(path.join(serverDir, 'code-acp.cjs'))).digest('hex');
  const metaSha = crypto.createHash('sha256').update(fs.readFileSync(path.join(serverDir, 'code-meta.cjs'))).digest('hex');
  const out = {};
  for (const [name, sc] of Object.entries(SCENARIOS)) { process.stderr.write(`scenario ${name}\n`); out[name] = await run(name, sc); }
  process.stdout.write(JSON.stringify({ generator: 'crates/code-acp/tools/gen-fixtures.cjs', sources: { 'code-acp.cjs': sha, 'code-meta.cjs': metaSha },
    env: { PATH: process.env.PATH, LANG: process.env.LANG }, scenarios: out }, null, 1) + '\n');
  clearInterval(keepAlive);
})();
