'use strict';
// Writes tests/fixtures/relay-node.v1.json: core's own code-machine.cjs `createEngineRelay` (over a real node:http
// server, so every answer is what the sandbox sees) and chat-machines.cjs `createChatMachines` (its `ready`,
// `contextTokens` and `connectError` over core's own machine-health.cjs and a scripted inference machine on
// loopback, plus the pure `modelIds` / `propsFactsOf`), each driven through the scenario table below. The Rust
// replays read the same file: crates/code-engine-relay/tests/node_relay.rs (the relay half, against `Relay`) and
// bins/noevia-server/tests/code_machine_node.rs (the machines half, against `FrontMachines` and the pure helpers).
// Council p11 (p9-1 CONTINUE 2, noevia#2676/#2720 item 4).
//
// Usage: node crates/code-engine-relay/tools/record-relay.cjs <noevia-core>/server > crates/code-engine-relay/tests/fixtures/relay-node.v1.json
//        node crates/code-engine-relay/tools/record-relay.cjs <noevia-core>/server --check crates/code-engine-relay/tests/fixtures/relay-node.v1.json
//          records again and exits 1 unless the result is byte-identical to the committed file.
// The core commit recorded against goes in tests/fixtures/core-ref. Synthetic data only: QA names, *.invalid hosts,
// loopback machines. Every wait is bounded (DEADLINE_MS); the whole run is bounded by RUN_DEADLINE_MS.

const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');

const args = process.argv.slice(2);
const checkAt = args.indexOf('--check');
const checkFile = checkAt >= 0 ? args[checkAt + 1] : null;
const positional = args.filter((a, i) => a !== '--check' && !(checkAt >= 0 && i === checkAt + 1));
const serverDir = path.resolve(positional[0] || '');
const { createEngineRelay } = require(path.join(serverDir, 'code-machine.cjs'));
const { createChatMachines, modelIds, propsFactsOf } = require(path.join(serverDir, 'chat-machines.cjs'));
const { createMachineHealth } = require(path.join(serverDir, 'machine-health.cjs'));

const DEADLINE_MS = 5000;
const RUN_DEADLINE_MS = 120000;
const runTimer = setTimeout(() => { process.stderr.write('record-relay: run deadline reached\n'); process.exit(3); }, RUN_DEADLINE_MS);
runTimer.unref();

const tick = (ms) => new Promise((r) => setTimeout(r, ms));
async function until(what, cond) {
  const end = Date.now() + DEADLINE_MS;
  while (!cond()) {
    if (Date.now() > end) throw Error(`record-relay: timed out waiting for ${what}`);
    await tick(5);
  }
}

// ---- the relay half -----------------------------------------------------------------------------------------------
const MACHINE = { id: 'qa-m', name: 'QA machine' };
const T1 = { machine: MACHINE, baseUrl: 'http://qa-machine.invalid:8080', model: 'synthetic-coder', port: 8080, role: 'code' };
const T2 = { machine: { id: 'qa-m2', name: 'QA machine two' }, baseUrl: 'http://qa-machine-2.invalid:8080', model: 'synthetic-coder', port: 8080, role: 'code' };
const T1_OTHER_MODEL = { ...T1, model: 'synthetic-coder-b' };
const TARGETS = { t1: T1, t2: T2, t1b: T1_OTHER_MODEL, none: null };
const READY_OK = { ok: true };
const READY_DOWN = { ok: false, code: 'machineUnreachable', error: 'QA: the machine is not answering.' };
const READY_WRONG = { ok: false, code: 'machineWrongModel', error: 'QA: the machine serves another model.' };
const READY_NO_CODE = { ok: false, error: 'QA: not ready, no code.' };
const READY_NO_ERROR = { ok: false, code: 'machineUnreachable' };
const READIES = { ok: READY_OK, down: READY_DOWN, wrong: READY_WRONG, noCode: READY_NO_CODE, noError: READY_NO_ERROR };

// Upstream scripts: what the fake machine's fetch gives. `chunks` are sent in order, then the body ends; `hold`
// keeps the body open after the chunks until the scenario's `release` step (or the request is cut); `fail` makes
// fetch reject with that error shape; `hang` never answers (the request's own timeout or the lease's close ends it).
const UPSTREAMS = {
  json: { status: 200, contentType: 'application/json', chunks: ['{"id":"qa-1","choices":[{"message":{"content":"QA"}}]}'] },
  sse: { status: 200, contentType: 'text/event-stream', chunks: ['data: {"choices":[{"delta":{"content":"QA"}}]}\n\n', 'data: [DONE]\n\n'] },
  noType: { status: 200, contentType: null, chunks: ['{"ok":true}'] },
  status500: { status: 500, contentType: 'application/json', chunks: ['{"error":{"message":"QA upstream failure"}}'] },
  status429: { status: 429, contentType: 'text/plain', chunks: ['QA busy'] },
  empty: { status: 204, contentType: 'application/json', chunks: [] },
  hold: { status: 200, contentType: 'text/event-stream', chunks: ['data: {"choices":[{"delta":{"content":"QA"}}]}\n\n'], hold: true },
  refused: { fail: { code: 'ECONNREFUSED', message: 'fetch failed' } },
  publicAddr: { fail: { code: 'EPUBLICADDR', message: 'not on this network' } },
  redirect: { fail: { message: 'unexpected redirect' } },
  hang: { hang: true },
};

// The fake machine's connectError, the same formula the Rust replay's fake uses: what reaches it is the relay's
// decision (an error, or the request's time running out), not chat-machines' own wording (the machines half
// records that).
const connectText = (t, err) => (err?.name === 'TimeoutError' ? `QA connect: timeout on ${t.machine.id}`
  : `QA connect: ${err?.code || 'error'} on ${t.machine.id}`);

/*
 * Each scenario: `options` (createEngineRelay's bodyCap / maxConcurrent / requestTimeoutMs; `url` null = no relay),
 * `target` (a TARGETS key), `ready` (a READIES key, or a list used in order, the last repeated), `context` (what
 * contextTokens answers; 'throw' throws), `upstream` (an UPSTREAMS key) and `steps`:
 *   { open: { taskId, model?, expect? } }   expect: a TARGETS key whose targetKey is passed (or 'none' for null)
 *   { request: { method, path, auth, accept?, body?, bodyBytes? } }
 *       auth: 'lease' (the last opened token), 'lease:<n>' (the n-th), 'none', 'basic', 'lowercase' (bearer + token),
 *             'tab' (Bearer\t<token>), 'stale' (a token never minted)
 *       body: a string sent as is; bodyBytes: that many bytes of 'a' (the cap cases)
 *   { hold: n }        n chat requests whose answers stay open (upstream must be 'hold')
 *   { release: true }  ends the held answers
 *   { move: <TARGETS key> }
 *   { close: <n> }     closes the n-th opened lease
 *   { active: true }   records the relay's open leases
 */
const POST = (body, extra = {}) => ({ request: { method: 'POST', path: '/v1/chat/completions', auth: 'lease', body, ...extra } });
const CHAT = JSON.stringify({ model: 'whatever-the-harness-says', stream: true, messages: [{ role: 'user', content: 'QA hello' }] });
const RELAY_SCENARIOS = [
  { name: 'open_and_list_the_model', target: 't1', ready: 'ok', context: 32768, upstream: 'json', steps: [
    { open: { taskId: 'qa-task-1' } }, { active: true },
    { request: { method: 'GET', path: '/v1/models', auth: 'lease' } },
    { request: { method: 'GET', path: '/models?x=1', auth: 'lease' } },
    { close: 0 }, { active: true },
    { request: { method: 'GET', path: '/v1/models', auth: 'lease' } }] },
  { name: 'open_refusals', target: 't1', ready: 'ok', context: null, upstream: 'json', steps: [
    { open: { taskId: 'qa-task-2', model: 'synthetic-coder-other' } },
    { open: { taskId: 'qa-task-2', expect: 't2' } },
    { open: { taskId: 'qa-task-2', expect: 'none' } },
    { open: { taskId: 'qa-task-2', expect: 't1', model: 'synthetic-coder' } },
    { active: true }] },
  { name: 'open_with_no_target', target: 'none', ready: 'ok', context: null, upstream: 'json', steps: [
    { open: { taskId: 'qa-task-3' } }, { active: true }] },
  { name: 'open_with_no_relay_url', options: { url: null }, target: 't1', ready: 'ok', context: null, upstream: 'json', steps: [
    { open: { taskId: 'qa-task-4' } }] },
  { name: 'open_machine_not_ready', target: 't1', ready: ['down', 'wrong', 'noCode', 'noError', 'ok'], context: 'throw', upstream: 'json', steps: [
    { open: { taskId: 'qa-task-5' } }, { open: { taskId: 'qa-task-5' } }, { open: { taskId: 'qa-task-5' } },
    { open: { taskId: 'qa-task-5' } }, { open: { taskId: 'qa-task-5' } }, { active: true }] },
  { name: 'tokens_and_paths', target: 't1', ready: 'ok', context: 4096, upstream: 'json', steps: [
    { open: { taskId: 'qa-task-6' } },
    { request: { method: 'GET', path: '/v1/models', auth: 'none' } },
    { request: { method: 'GET', path: '/v1/models', auth: 'stale' } },
    { request: { method: 'GET', path: '/v1/models', auth: 'basic' } },
    { request: { method: 'GET', path: '/v1/models', auth: 'lowercase' } },
    { request: { method: 'GET', path: '/v1/models', auth: 'tab' } },
    { request: { method: 'POST', path: '/v1/models', auth: 'lease', body: '{}' } },
    { request: { method: 'GET', path: '/v1/chat/completions', auth: 'lease' } },
    { request: { method: 'POST', path: '/v1/completions', auth: 'lease', body: '{}' } },
    { request: { method: 'POST', path: '/v1/embeddings', auth: 'lease', body: '{}' } },
    { request: { method: 'DELETE', path: '/v1/chat/completions', auth: 'lease' } },
    { request: { method: 'POST', path: '/v1/chat/completions/', auth: 'lease', body: '{}' } }] },
  { name: 'bodies', target: 't1', ready: 'ok', context: null, upstream: 'json', options: { bodyCap: 64 }, steps: [
    { open: { taskId: 'qa-task-7' } },
    POST('not json'), POST(''), POST('[1,2]'), POST('null'), POST('"text"'), POST('7'),
    POST(null, { bodyBytes: 65 }), POST(null, { bodyBytes: 64 }),
    POST('{"messages":[]}')] },
  { name: 'forward_forces_the_model', target: 't1', ready: 'ok', context: null, upstream: 'json', steps: [
    { open: { taskId: 'qa-task-8' } },
    POST(CHAT, { accept: 'text/event-stream' }),
    POST('{"messages":[],"temperature":0.2}'),
    POST('{"model":null,"messages":[]}', { accept: `application/json, ${'x'.repeat(240)}` }),
    POST(CHAT, { path: '/chat/completions?stream=1' })] },
  { name: 'upstream_sse', target: 't1', ready: 'ok', context: null, upstream: 'sse', steps: [
    { open: { taskId: 'qa-task-9' } }, POST(CHAT, { accept: 'text/event-stream' })] },
  { name: 'upstream_no_content_type', target: 't1', ready: 'ok', context: null, upstream: 'noType', steps: [
    { open: { taskId: 'qa-task-10' } }, POST(CHAT)] },
  { name: 'upstream_error_statuses_pass_through', target: 't1', ready: 'ok', context: null, upstream: 'status500', steps: [
    { open: { taskId: 'qa-task-11' } }, POST(CHAT)] },
  { name: 'upstream_429_text', target: 't1', ready: 'ok', context: null, upstream: 'status429', steps: [
    { open: { taskId: 'qa-task-12' } }, POST(CHAT)] },
  { name: 'upstream_empty_204', target: 't1', ready: 'ok', context: null, upstream: 'empty', steps: [
    { open: { taskId: 'qa-task-13' } }, POST(CHAT)] },
  { name: 'upstream_refused', target: 't1', ready: 'ok', context: null, upstream: 'refused', steps: [
    { open: { taskId: 'qa-task-14' } }, POST(CHAT)] },
  { name: 'upstream_public_address', target: 't1', ready: 'ok', context: null, upstream: 'publicAddr', steps: [
    { open: { taskId: 'qa-task-15' } }, POST(CHAT)] },
  { name: 'upstream_redirect', target: 't1', ready: 'ok', context: null, upstream: 'redirect', steps: [
    { open: { taskId: 'qa-task-16' } }, POST(CHAT)] },
  { name: 'upstream_times_out', target: 't1', ready: 'ok', context: null, upstream: 'hang', options: { requestTimeoutMs: 150 }, steps: [
    { open: { taskId: 'qa-task-17' } }, POST(CHAT)] },
  { name: 'role_moved_during_the_task', target: 't1', ready: 'ok', context: null, upstream: 'json', steps: [
    { open: { taskId: 'qa-task-18' } }, POST(CHAT),
    { move: 't2' }, POST(CHAT), { request: { method: 'GET', path: '/v1/models', auth: 'lease' } },
    { move: 't1b' }, POST(CHAT),
    { move: 'none' }, POST(CHAT),
    { move: 't1' }, POST(CHAT)] },
  { name: 'machine_stops_being_ready', target: 't1', ready: ['ok', 'ok', 'down', 'noError', 'ok'], context: null, upstream: 'json', steps: [
    { open: { taskId: 'qa-task-19' } }, POST(CHAT), POST(CHAT), POST(CHAT), POST(CHAT)] },
  { name: 'four_at_once_then_429', target: 't1', ready: 'ok', context: null, upstream: 'hold', steps: [
    { open: { taskId: 'qa-task-20' } }, { hold: 4 }, POST(CHAT), { request: { method: 'GET', path: '/v1/models', auth: 'lease' } },
    { release: true }, POST(CHAT)] },
  { name: 'max_concurrent_one', target: 't1', ready: 'ok', context: null, upstream: 'hold', options: { maxConcurrent: 1 }, steps: [
    { open: { taskId: 'qa-task-21' } }, { hold: 1 }, POST(CHAT), { release: true }, POST(CHAT)] },
  { name: 'two_leases_one_closed', target: 't1', ready: 'ok', context: null, upstream: 'json', steps: [
    { open: { taskId: 'qa-task-22a' } }, { open: { taskId: 'qa-task-22b' } }, { active: true },
    { close: 0 }, { active: true },
    { request: { method: 'GET', path: '/v1/models', auth: 'lease:0' } },
    { request: { method: 'GET', path: '/v1/models', auth: 'lease:1' } },
    { close: 0 }, { close: 1 }, { active: true }] },
];

function scriptedMachines(sc, world) {
  const readies = Array.isArray(sc.ready) ? sc.ready : [sc.ready];
  let readyAt = 0;
  return {
    ready: async (t) => {
      world.readyCalls.push(t ? t.machine.id : null);
      const r = READIES[readies[Math.min(readyAt, readies.length - 1)]];
      readyAt += 1;
      return { ...r };
    },
    contextTokens: async () => { if (sc.context === 'throw') throw Error('QA: no props'); return sc.context; },
    beginTurn: (id) => { world.turns.push(`begin:${id}`); let done = false; return () => { if (!done) { done = true; world.turns.push(`end:${id}`); } }; },
    connectError: (t, err) => connectText(t, err),
    fetch: async (url, init) => {
      const h = init.headers || {};
      world.sent.push({ url, method: init.method, contentType: h['Content-Type'] ?? null, accept: h.Accept ?? null, body: init.body });
      const up = UPSTREAMS[sc.upstream];
      if (up.fail) throw Object.assign(Error(up.fail.message), up.fail.code ? { code: up.fail.code } : {});
      if (up.hang) {
        return new Promise((_, reject) => {
          const sig = init.signal;
          const onAbort = () => reject(sig.reason ?? Object.assign(Error('aborted'), { name: 'AbortError' }));
          if (sig.aborted) onAbort(); else sig.addEventListener('abort', onAbort, { once: true });
        });
      }
      const enc = new TextEncoder();
      let held = null;
      const stream = new ReadableStream({
        start(controller) {
          for (const c of up.chunks) controller.enqueue(enc.encode(c));
          // Only the requests a `hold` step starts stay open; every other request's answer ends.
          if (up.hold && world.holdNext > 0) { world.holdNext -= 1; held = controller; world.held.push(() => { try { controller.close(); } catch { /* closed */ } }); }
          else controller.close();
        },
        cancel() { if (held) held = null; },
      });
      const headers = new Headers();
      if (up.contentType) headers.set('content-type', up.contentType);
      return new Response(up.status === 204 ? null : stream, { status: up.status, headers });
    },
  };
}

function send(port, { method, path: p, headers, body }) {
  return new Promise((resolve, reject) => {
    const req = http.request({ host: '127.0.0.1', port, method, path: p, headers, agent: false }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => resolve({ status: res.statusCode, contentType: res.headers['content-type'] ?? null,
        cacheControl: res.headers['cache-control'] ?? null, body: Buffer.concat(chunks).toString('utf8') }));
      res.on('error', () => resolve({ cut: true }));
      res.on('aborted', () => resolve({ cut: true }));
      res.on('close', () => { if (!res.complete) resolve({ cut: true }); });
    });
    req.on('error', (e) => (e.code === 'ECONNRESET' || e.message === 'socket hang up' ? resolve({ cut: true }) : reject(e)));
    req.setTimeout(DEADLINE_MS, () => { req.destroy(Error('record-relay: request deadline')); });
    if (body !== undefined && body !== null) req.write(body);
    req.end();
  });
}

function authOf(spec, tokens) {
  const last = tokens[tokens.length - 1] || 'ncr_none';
  if (spec === 'none') return undefined;
  if (spec === 'lease') return `Bearer ${last}`;
  if (spec.startsWith('lease:')) return `Bearer ${tokens[Number(spec.slice(6))]}`;
  if (spec === 'basic') return `Basic ${last}`;
  if (spec === 'lowercase') return `bearer ${last}`;
  if (spec === 'tab') return `Bearer\t${last}`;
  if (spec === 'stale') return 'Bearer ncr_QA_never_minted_token_000000000';
  throw Error(`record-relay: unknown auth ${spec}`);
}

async function recordRelay(sc) {
  const world = { sent: [], turns: [], readyCalls: [], held: [], holdNext: 0, log: [] };
  let current = TARGETS[sc.target];
  const o = sc.options || {};
  const relay = createEngineRelay({ chatMachines: scriptedMachines(sc, world), target: () => current,
    url: 'url' in o ? o.url : 'http://qa-relay.invalid:8095/v1', log: (e) => world.log.push(e),
    ...(o.bodyCap ? { bodyCap: o.bodyCap } : {}), ...(o.maxConcurrent ? { maxConcurrent: o.maxConcurrent } : {}),
    ...(o.requestTimeoutMs ? { requestTimeoutMs: o.requestTimeoutMs } : {}) });
  const server = http.createServer((req, res) => { relay.handle(req, res).catch(() => { try { res.destroy(); } catch { /* gone */ } }); });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const port = server.address().port;
  const leases = [], tokens = [], out = [], pending = [];
  try {
    for (const step of sc.steps) {
      if (step.open) {
        const { taskId, model, expect } = step.open;
        const expectKey = expect === undefined ? undefined : (TARGETS[expect] ? `${TARGETS[expect].machine.id}|${TARGETS[expect].baseUrl}|${TARGETS[expect].model}` : null);
        try {
          const l = await relay.open({ taskId, ...(model !== undefined ? { model } : {}), ...(expect !== undefined ? { expect: expectKey } : {}) });
          leases.push(l); tokens.push(l.token);
          out.push({ open: { ok: true, baseUrl: l.baseUrl, model: l.model, contextTokens: l.contextTokens ?? null,
            machine: l.target.machine.id, tokenShape: /^ncr_[A-Za-z0-9_-]{32}$/.test(l.token) } });
        } catch (e) {
          out.push({ open: { ok: false, code: e.code ?? null, message: e.publicMessage ?? e.message } });
        }
      } else if (step.request) {
        const r = step.request;
        const headers = {};
        const auth = authOf(r.auth, tokens);
        if (auth !== undefined) headers.authorization = auth;
        if (r.accept) headers.accept = r.accept;
        const body = r.bodyBytes !== undefined ? 'a'.repeat(r.bodyBytes) : r.body;
        if (body !== undefined && body !== null) headers['content-type'] = 'application/json';
        const sentBefore = world.sent.length;
        const res = await send(port, { method: r.method, path: r.path, headers, body });
        out.push({ request: res, forwarded: world.sent.slice(sentBefore) });
      } else if (step.hold) {
        const before = world.sent.length;
        world.holdNext = step.hold;
        for (let i = 0; i < step.hold; i += 1) {
          pending.push(send(port, { method: 'POST', path: '/v1/chat/completions', headers: { authorization: authOf('lease', tokens), 'content-type': 'application/json' }, body: CHAT }));
        }
        await until('the held requests to reach the machine', () => world.sent.length >= before + step.hold && world.held.length >= step.hold);
        out.push({ hold: { forwarded: world.sent.length - before } });
      } else if (step.release) {
        for (const end of world.held.splice(0)) end();
        const answers = await Promise.all(pending.splice(0));
        // Give the relay's finally blocks their turn before the next step reads the slots.
        await until('the held requests to give their slots back', () => world.turns.filter((t) => t.startsWith('end:')).length >= world.turns.filter((t) => t.startsWith('begin:')).length);
        out.push({ release: answers });
      } else if ('move' in step) {
        current = TARGETS[step.move];
        out.push({ move: step.move });
      } else if ('close' in step) {
        leases[step.close].close();
        out.push({ close: step.close });
      } else if (step.active) {
        out.push({ active: relay.active() });
      } else {
        throw Error(`record-relay: unknown step ${JSON.stringify(step)}`);
      }
    }
  } finally {
    for (const end of world.held.splice(0)) end();
    await Promise.allSettled(pending);
    for (const l of leases) l.close();
    await new Promise((r) => server.close(r));
  }
  // The log without anything secret (a token is never logged; checked here so a change is noticed).
  const log = world.log.map((e) => ({ ...e }));
  if (JSON.stringify(log).includes('ncr_')) throw Error('record-relay: a relay token reached the log');
  return { name: sc.name, options: sc.options || {}, target: sc.target, ready: sc.ready, context: sc.context, upstream: sc.upstream,
    steps: sc.steps, outcomes: out, log, turns: world.turns, readyCalls: world.readyCalls };
}

// ---- the machines half --------------------------------------------------------------------------------------------
// A scripted inference machine on loopback: /health, /v1/models and /props answer as the scenario says. The probe
// (machine-health.cjs) and the facts read (chat-machines.cjs) use a plain fetch here (core's serviceFetch refuses
// loopback); the on-this-network rules are service-endpoint.cjs's and model-stream's, recorded elsewhere.
const MACHINE_SCENARIOS = [
  { name: 'up_and_serving', health: 200, models: { status: 200, body: '{"data":[{"id":"qa-coder"},{"id":"qa-other"}]}' },
    props: { status: 200, body: '{"default_generation_settings":{"n_ctx":65536}}' } },
  { name: 'up_needs_auth', health: 401, models: { status: 200, body: '{"data":[{"id":"qa-coder"}]}' }, props: { status: 404, body: '' } },
  { name: 'up_forbidden', health: 403, models: { status: 200, body: '{"data":["qa-coder"]}' }, props: { status: 200, body: '{}' } },
  { name: 'down_503', health: 503, models: { status: 200, body: '{"data":[{"id":"qa-coder"}]}' }, props: { status: 200, body: '{}' } },
  { name: 'down_404', health: 404, models: { status: 200, body: '{"data":[{"id":"qa-coder"}]}' }, props: { status: 200, body: '{}' } },
  { name: 'down_500', health: 500, models: { status: 500, body: '' }, props: { status: 500, body: '' } },
  { name: 'wrong_model', health: 200, models: { status: 200, body: '{"data":[{"id":"qa-a"},{"id":"qa-b"}]}' }, props: { status: 200, body: '{}' } },
  { name: 'wrong_model_many', health: 200, models: { status: 200, body: '{"data":[{"id":"qa-a"},{"id":"qa-b"},{"id":"qa-c"},{"id":"qa-d"}]}' }, props: { status: 200, body: '{}' } },
  { name: 'wrong_model_none_served', health: 200, models: { status: 200, body: '{"data":[]}' }, props: { status: 200, body: '{}' } },
  { name: 'models_unreadable_blames_nothing', health: 200, models: { status: 500, body: '' }, props: { status: 200, body: '{"default_generation_settings":{"n_ctx":4096}}' } },
  { name: 'models_not_json', health: 200, models: { status: 200, body: 'QA not json' }, props: { status: 200, body: 'QA not json' } },
  { name: 'models_older_shape', health: 200, models: { status: 200, body: '{"models":[{"name":"qa-coder"},{"model":"qa-x"}]}' }, props: { status: 200, body: '{"default_generation_settings":{"n_ctx":"8192"}}' } },
  { name: 'context_out_of_range', health: 200, models: { status: 200, body: '{"data":[{"id":"qa-coder"}]}' }, props: { status: 200, body: '{"default_generation_settings":{"n_ctx":1024}}' } },
  { name: 'context_fraction', health: 200, models: { status: 200, body: '{"data":[{"id":"qa-coder"}]}' }, props: { status: 200, body: '{"default_generation_settings":{"n_ctx":4096.5}}' } },
  { name: 'context_hex_text', health: 200, models: { status: 200, body: '{"data":[{"id":"qa-coder"}]}' }, props: { status: 200, body: '{"default_generation_settings":{"n_ctx":"0x4000"}}' } },
  { name: 'context_one_element_array', health: 200, models: { status: 200, body: '{"data":[{"id":"qa-coder"}]}' }, props: { status: 200, body: '{"default_generation_settings":{"n_ctx":[16384]}}' } },
];
const MACHINE_TARGET = { role: 'code', machine: { id: 'qa-box', name: 'QA Box' }, model: 'qa-coder' };

function scriptedMachine(sc) {
  const seen = [];
  const server = http.createServer((req, res) => {
    const p = String(req.url || '').split('?')[0];
    seen.push(`${req.method} ${req.url}`);
    req.resume();
    const answer = (status, body, type = 'application/json') => {
      res.writeHead(status, { 'content-type': type, 'content-length': Buffer.byteLength(body) });
      res.end(body);
    };
    if (p === '/health') return answer(sc.health, '');
    if (p === '/v1/models') return answer(sc.models.status, sc.models.body);
    if (p === '/props') return answer(sc.props.status, sc.props.body);
    return answer(404, '');
  });
  return { server, seen };
}

async function recordMachine(sc) {
  const { server, seen } = scriptedMachine(sc);
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const port = server.address().port;
  const t = { ...MACHINE_TARGET, machine: { ...MACHINE_TARGET.machine, address: 'http://127.0.0.1' }, port, baseUrl: `http://127.0.0.1:${port}` };
  const plain = (u, init = {}) => fetch(u, init);
  try {
    const health = createMachineHealth({ fetchImpl: plain, decisionFetchImpl: plain });
    const cm = createChatMachines({ machines: { chatTarget: () => t, machine: () => t.machine }, health, fetchImpl: plain });
    const ready = await cm.ready(t);
    const contextTokens = await cm.contextTokens(t);
    // The port is the run's own: the texts name it, so it is written as {port} and the replay puts its own back.
    const unport = (s) => (typeof s === 'string' ? s.split(String(port)).join('{port}') : s);
    return { name: sc.name, health: sc.health, models: sc.models, props: sc.props,
      ready: { ok: ready.ok, ...(ready.code !== undefined ? { code: ready.code } : {}), ...(ready.error !== undefined ? { error: unport(ready.error) } : {}) },
      contextTokens, paths: [...new Set(seen.map((s) => s.split('?')[0]))].sort() };
  } finally {
    await new Promise((r) => server.close(r));
  }
}

// connectError's reasons over the error shapes a fetch can reject with.
const CONNECT_ERRORS = [
  { name: 'public_address', err: { code: 'EPUBLICADDR', message: 'not on this network' } },
  { name: 'redirect', err: { message: 'unexpected redirect' } },
  { name: 'redirect_upper', err: { message: 'REDIRECT refused' } },
  { name: 'timeout', err: { name: 'TimeoutError', message: 'The operation was aborted due to timeout' } },
  { name: 'refused', err: { code: 'ECONNREFUSED', message: 'fetch failed' } },
  { name: 'none', err: null },
];
const CONNECT_TARGET = { role: 'code', machine: { id: 'qa-box', name: 'QA Box' }, model: 'qa-coder', port: 8080, baseUrl: 'http://10.0.0.5:8080' };

function recordConnectErrors() {
  const cm = createChatMachines({ machines: { chatTarget: () => CONNECT_TARGET, machine: () => CONNECT_TARGET.machine } });
  return CONNECT_ERRORS.map(({ name, err }) => {
    const e = err ? Object.assign(Error(err.message), err.code ? { code: err.code } : {}) : null;
    if (e && err.name) Object.defineProperty(e, 'name', { value: err.name });
    return { name, err, text: cm.connectError(CONNECT_TARGET, e) };
  });
}

// The pure readers, over bodies a machine might send.
const MODEL_BODIES = [
  '{"data":[{"id":"qa-a"},{"id":"qa-b"}]}', '{"data":["qa-a","qa-a","qa-b"]}', '{"models":[{"name":"qa-n"},{"model":"qa-m"},{"id":"qa-i","name":"qa-x"}]}',
  '{"data":{"id":"qa-a"},"models":[{"name":"qa-fallback"}]}', '{"data":[{"id":null,"model":"qa-m"},{"id":5},{"id":""},{"id":"  "}]}',
  '{"data":[{"id":"qa\\u0001ctl"},{"id":"qa\\u007fdel"},{"id":"qa-ok"}]}', '{"data":[{"id":"\\ufeff"},{"id":"\\u00a0"},{"id":"qa\\u2028ls"}]}',
  `{"data":[{"id":"${'q'.repeat(200)}"},{"id":"${'q'.repeat(201)}"}]}`, '[]', 'null', '"qa"', '{"data":[[1],{"id":["qa"]}]}',
  `{"data":[${Array.from({ length: 205 }, (_, i) => `"qa-${i}"`).join(',')}]}`,
];
const PROPS_BODIES = [
  '{"default_generation_settings":{"n_ctx":4096}}', '{"default_generation_settings":{"n_ctx":2048}}', '{"default_generation_settings":{"n_ctx":2047}}',
  '{"default_generation_settings":{"n_ctx":2000000}}', '{"default_generation_settings":{"n_ctx":2000001}}', '{"default_generation_settings":{"n_ctx":"8192"}}',
  '{"default_generation_settings":{"n_ctx":" 8192 "}}', '{"default_generation_settings":{"n_ctx":"0x4000"}}', '{"default_generation_settings":{"n_ctx":"0b100000000000"}}',
  '{"default_generation_settings":{"n_ctx":"0o20000"}}', '{"default_generation_settings":{"n_ctx":"8e3"}}', '{"default_generation_settings":{"n_ctx":"+4096"}}',
  '{"default_generation_settings":{"n_ctx":"4096."}}', '{"default_generation_settings":{"n_ctx":"4_096"}}', '{"default_generation_settings":{"n_ctx":[16384]}}',
  '{"default_generation_settings":{"n_ctx":["16384"]}}', '{"default_generation_settings":{"n_ctx":[16384,1]}}', '{"default_generation_settings":{"n_ctx":true}}',
  '{"default_generation_settings":{"n_ctx":null}}', '{"default_generation_settings":{"n_ctx":4096.5}}', '{"default_generation_settings":{}}', '{}', 'null',
  '{"default_generation_settings":{"n_ctx":"Infinity"}}', '{"default_generation_settings":{"n_ctx":"\\u00a04096\\ufeff"}}', '{"default_generation_settings":{"n_ctx":{"v":1}}}',
];

async function main() {
  const relay = [];
  for (const sc of RELAY_SCENARIOS) relay.push(await recordRelay(sc));
  const machines = [];
  for (const sc of MACHINE_SCENARIOS) machines.push(await recordMachine(sc));
  const fixture = {
    version: 1,
    source: 'noevia-core server/code-machine.cjs createEngineRelay + server/chat-machines.cjs (ready, contextTokens, connectError, modelIds, propsFactsOf) + server/machine-health.cjs; recorded by crates/code-engine-relay/tools/record-relay.cjs',
    targets: TARGETS, readies: READIES, upstreams: UPSTREAMS,
    relay,
    machines: { target: MACHINE_TARGET, scenarios: machines },
    connectErrors: { target: CONNECT_TARGET, cases: recordConnectErrors() },
    modelIds: MODEL_BODIES.map((body) => ({ body, ids: modelIds(JSON.parse(body)) })),
    propsContext: PROPS_BODIES.map((body) => ({ body, contextTokens: propsFactsOf(JSON.parse(body)).contextTokens })),
  };
  const text = `${JSON.stringify(fixture, null, 1)}\n`;
  if (checkFile) {
    const committed = fs.readFileSync(checkFile, 'utf8');
    if (committed !== text) { process.stderr.write(`record-relay: ${checkFile} differs from a fresh recording\n`); process.exit(1); }
    process.stderr.write('record-relay: the committed fixture matches a fresh recording\n');
  } else {
    process.stdout.write(text);
  }
}

main().then(() => { clearTimeout(runTimer); }, (e) => { process.stderr.write(`${e.stack || e}\n`); process.exit(2); });
