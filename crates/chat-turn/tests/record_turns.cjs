#!/usr/bin/env node
'use strict';
// Recorded chat turns for crates/chat-turn: the contract the native turn is replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout with server/wasm/dav-parse.wasm built> \
//     node crates/chat-turn/tests/record_turns.cjs > crates/chat-turn/tests/fixtures/node-turns.v1.json
//   ... node crates/chat-turn/tests/record_turns.cjs --check crates/chat-turn/tests/fixtures/node-turns.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every case runs one POST /api/chat through noevia-core's own server/chat.cjs (createChatHandler
// → handleChat), with the real context, partial, interrupted-turn, effort, thinking-cap, toolbox,
// Drive-tool and tool-exchange modules. Only the edges are stubs, and each records what the turn
// asked of it: the engine (a scripted fetch answering with real Response objects), the model
// manager (one loaded model, a configured context size), the approval gate (a fixed answer), the
// stores (chat list, titles, transcript append) and the audit log. Everything is synthetic and
// QA-named; the engine is a .invalid host and nothing touches the network.
//
// Deterministic by construction: the clock is frozen at FIXED_NOW (TZ=UTC), crypto.randomUUID is a
// counter, and every *_IMPL switch is left at its default in code. The engine is configured as a llama.cpp
// deployment's (MODEL_MANAGER_KIND=llamacpp, INFERENCE_BASE_URL = the engine), so the default provider is the
// local engine; the model manager grants the inference gate's lease at once (X-Noevia-Lease on every request).
// What a case records:
//   http      the status and headers of the reply, or the JSON refusal sent instead of a stream
//   events    every SSE `data:` object in order (a keep-alive comment fails the recording: no case runs 5 s)
//   requests  every engine request: url, headers, the parsed body, and whether it was the chat endpoint
//   stores    the calls into the stores (saveChats, chatTitles.save, transcripts.appendIfLast, ...)
//   files     the workspace files after the turn (context state, partial), parsed, run-local ids out
//   record    createTurnRecord() fed with the events: the transcript entry the client keeps
//   handoff   (a core with rust-chat-native.cjs) the same turn asked for by the front's native executor:
//             { prepared: the frame | null, refused: the logged reason | null, before: events before the frame,
//               requests: engine requests made before it }
// The tool list a request offers is long and repeats: each distinct list is kept once under `tools` at the top,
// and a request body's `tools` is { "$tools": <key> } (the first 16 hex of the list's JSON sha256).
// The source files' sha256 are checked, so a changed Node file needs a fresh recording.

process.env.TZ = 'UTC';
const checkout = process.env.NOEVIA_CORE_CHECKOUT;
for (const key of Object.keys(process.env)) if (/_IMPL$/.test(key) || /^(CONTEXT_LOG|MODEL_MANAGER_KIND|INFERENCE_BASE_URL|NOEVIA_)/.test(key)) delete process.env[key];
// A llama.cpp deployment: the default provider is the local engine (reasoning-effort.cjs localEngine).
process.env.MODEL_MANAGER_KIND = 'llamacpp';
process.env.INFERENCE_BASE_URL = 'http://qa-engine.invalid';
// The supervisor's per-start id (chat-partial.cjs START_ID), fixed: it is in every grant binding.
process.env.NOEVIA_START_ID = '0123456789abcdef0123456789abcdef';

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { EventEmitter } = require('node:events');

if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const core = path.resolve(checkout);
const server = path.join(core, 'server');
if (!process.env.DAV_PARSE_WASM && !fs.existsSync(path.join(server, 'wasm', 'dav-parse.wasm'))) {
  process.stderr.write('Build server/wasm/dav-parse.wasm in the checkout first (tools/build-dav-parse-wasm.sh), or set DAV_PARSE_WASM.\n');
  process.exit(2);
}

// The files whose behaviour these recordings pin. A change to any of them needs a fresh recording.
const SOURCES = {
  'server/chat.cjs': null,
  'server/chat-context.cjs': null,
  'server/chat-partial.cjs': null,
  'server/chat-interrupted-turn.cjs': null,
  'server/chat-title.cjs': null,
  'server/final-answer.cjs': null,
  'server/reasoning-effort.cjs': null,
  'server/thinking-budget.cjs': null,
  'server/tool-exchange.cjs': null,
  'server/toolboxes.cjs': null,
  'server/announced-action.cjs': null,
  'server/search-query.cjs': null,
  'server/prompt-framing.cjs': null,
  'server/dav-parse.lock': null,
};
for (const rel of Object.keys(SOURCES)) {
  SOURCES[rel] = crypto.createHash('sha256').update(fs.readFileSync(path.join(core, rel))).digest('hex');
}
const PINNED = process.env.RECORD_SOURCES ? JSON.parse(fs.readFileSync(process.env.RECORD_SOURCES, 'utf8')) : null;
if (PINNED) {
  for (const [rel, want] of Object.entries(PINNED)) {
    if (SOURCES[rel] !== want) {
      process.stderr.write(`source hash mismatch for ${rel}: ${SOURCES[rel]}\n`);
      process.exit(1);
    }
  }
}

// The Node modules log to the console; stdout is the fixture, so their lines go to stderr.
// A turn's own lines are also kept while it runs (`logged`), for the hand-off's refusal reason.
let logged = [];
for (const level of ['log', 'info', 'warn', 'error']) console[level] = (...args) => { const line = args.map(String).join(' '); logged.push(line); process.stderr.write(`${line}\n`); };

// ── Determinism ─────────────────────────────────────────────────────────────────────────────
const FIXED_NOW = Date.UTC(2026, 10, 13, 12, 0, 0);
const RealDate = Date;
class FrozenDate extends RealDate {
  constructor(...args) { if (args.length) super(...args); else super(FIXED_NOW); }
  static now() { return FIXED_NOW; }
}
global.Date = FrozenDate;
let uuidCounter = 0;
const nextUuid = () => `00000000-0000-4000-8000-${String(++uuidCounter).padStart(12, '0')}`;
const fixedCrypto = Object.assign(Object.create(crypto), { randomUUID: nextUuid });
let mathSeed = 0x2026;
Math.random = () => { mathSeed = (mathSeed * 1103515245 + 12345) % 2147483648; return mathSeed / 2147483648; };

// The one Drive call a write makes before it runs is the metadata read that names the card's target
// (gdrive-tools.cjs describeTarget). gdrive-tools takes driveFiles when it loads, so it is replaced first.
require(path.join(server, 'gdrive-files.cjs')).driveFiles = () => ({
  metadata: async ({ fileId }) => ({ id: fileId, name: `QA file ${fileId}` }),
});
const { createChatHandler } = require(path.join(server, 'chat.cjs'));
const { createToolboxes } = require(path.join(server, 'toolboxes.cjs'));
const { createDriveTools } = require(path.join(server, 'gdrive-tools.cjs'));
const { createToolExchange } = require(path.join(server, 'tool-exchange.cjs'));
const { createTurnRecord } = require(path.join(server, 'chat-interrupted-turn.cjs'));
const grantFixture = require(path.join(core, 'tests', 'server', 'chat-grant-fixture.cjs'));

const ENGINE = 'http://qa-engine.invalid';
const CHAT_URL = `${ENGINE}/v1/chat/completions`;
const MODEL = 'qa-model';

// ── The scripted engine ─────────────────────────────────────────────────────────────────────
// A round is one engine request, answered in order whatever its URL:
//   { frames: [...] }   a 200 event stream; each frame is an object (sent as `data: <json>\n\n`), a raw string
//                       (sent as it is), or a marker: { leave: true } the client goes away here (the turn's
//                       `close`), { stall: true } no more bytes until the request is aborted, { done: true }
//                       `data: [DONE]`, { split: <object> } one frame cut in two chunks mid-line, { splitBytes: <object> }
//                       one frame cut inside the UTF-8 bytes of its first astral character.
//   { status, body }    a non-200 reply with this body text
//   { json }            a 200 non-streaming JSON reply (the empty-stream fallback, a summary, a title)
//   { throws }          the request fails before any reply
const enc = new TextEncoder();
function engine(rounds, log, res) {
  let next = 0;
  const fetch = async (url, options = {}) => {
    const body = typeof options.body === 'string' ? JSON.parse(options.body) : null;
    log.requests.push({ url: String(url), chat: String(url) === CHAT_URL, stream: body?.stream === true, headers: options.headers || {}, body });
    const round = rounds[next++];
    if (!round) { log.overrun = true; throw new Error(`the script has no engine round ${next}`); }
    if (round.throws) throw new Error(round.throws);
    if (round.status && round.status !== 200) return new Response(round.body ?? '', { status: round.status, headers: { 'Content-Type': 'application/json' } });
    if (round.json) return new Response(JSON.stringify(round.json), { status: 200, headers: { 'Content-Type': 'application/json' } });
    const signal = options.signal;
    const chunks = [];
    for (const frame of round.frames || []) {
      if (frame && frame.leave) chunks.push({ leave: true });
      else if (frame && frame.stall) chunks.push({ stall: true });
      else if (frame && frame.done) chunks.push('data: [DONE]\n\n');
      else if (frame && frame.split) { const line = `data: ${JSON.stringify(frame.split)}\n\n`; const at = Math.floor(line.length / 2); chunks.push(line.slice(0, at), line.slice(at)); }
      else if (frame && frame.splitBytes) {
        // Cut inside the first 4-byte UTF-8 sequence (an astral character), so the decoder must carry its bytes over.
        const bytes = enc.encode(`data: ${JSON.stringify(frame.splitBytes)}\n\n`);
        const at = bytes.findIndex((b) => b >= 0xf0) + 2;
        chunks.push(bytes.subarray(0, at), bytes.subarray(at));
      }
      else chunks.push(typeof frame === 'string' ? frame : `data: ${JSON.stringify(frame)}\n\n`);
    }
    let i = 0;
    const aborted = () => new Promise((_, reject) => {
      const fail = () => reject(signal.reason ?? Object.assign(new Error('This operation was aborted'), { name: 'AbortError' }));
      if (signal?.aborted) fail(); else signal?.addEventListener('abort', fail, { once: true });
    });
    const stream = new ReadableStream({
      async pull(controller) {
        if (i >= chunks.length) { controller.close(); return; }
        const chunk = chunks[i++];
        if (chunk && chunk.leave) {
          await settle();
          res.destroyed = true;
          res.emit('close');
          await aborted();
          return;
        }
        if (chunk && chunk.stall) { await settle(); await aborted(); return; }
        controller.enqueue(typeof chunk === 'string' ? enc.encode(chunk) : chunk);
      },
    });
    return new Response(stream, { status: 200, headers: { 'Content-Type': 'text/event-stream' } });
  };
  return { fetch, used: () => next };
}
// Leaving and stalling wait a moment first, so the chunks before them reach the turn (the thinking cap and the
// stream reader read ahead of the turn by a chunk or two).
const settle = () => new Promise((resolve) => setTimeout(resolve, 25));

// ── One turn ────────────────────────────────────────────────────────────────────────────────
function listFiles(dir) {
  const out = {};
  const walk = (at) => {
    for (const name of fs.readdirSync(at).sort()) {
      const full = path.join(at, name);
      if (fs.statSync(full).isDirectory()) walk(full);
      else {
        let value = fs.readFileSync(full, 'utf8');
        try { value = JSON.parse(value); } catch { /* kept as text */ }
        if (value && typeof value === 'object' && 'running' in value) value = { ...value, running: { start: '<start-id>' } };
        out[path.relative(dir, full).split(path.sep).join('/')] = value;
      }
    }
  };
  walk(dir);
  return out;
}

// `handoff`: the front's native executor asks for the turn (rust-chat-native.cjs, when the core has it). The turn
// then ends after its preparation with a `prepared` frame, or goes on in Node (the refusal is logged).
async function runTurn(c, { handoff = false } = {}) {
  uuidCounter = 0;
  mathSeed = 0x2026;
  logged = [];
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-chat-turn-'));
  try {
    const user = { id: 'qa-user', role: 'admin' };
    const project = c.project === null ? null : {
      id: 'qa-project', name: 'QA Project', model: MODEL, routing: 'manual', toolboxes: ['core'], files: [], ...(c.project || {}),
    };
    const log = { requests: [], stores: [], approvals: [], executions: [] };
    const store = (op, args) => log.stores.push({ op, ...args });
    const workspace = {
      userId: user.id, dir, assertActive() {},
      assetDir: () => path.join(dir, 'assets'),
      transcripts: {
        appendIfLast: async (chatId, message, entry, options) => { store('transcripts.appendIfLast', { chatId, message, entry, options }); return c.appendOutcome || 'saved'; },
        deleteChat: async (chatId, options) => { store('transcripts.deleteChat', { chatId, options }); },
      },
    };
    const scope = { workspace: { userId: user.id }, authn: { user } };
    const requestScope = { getStore: () => scope, run: (_s, fn) => fn() };
    // A connected Drive (its metadata read is the stub installed above, before gdrive-tools.cjs loaded).
    const accounts = { forUser: () => ({ drive: { state: () => ({ state: 'connected' }) } }) };
    const definitions = createDriveTools({ accounts });
    const driveTools = { ...definitions, execute: async (_user, tool) => {
      log.executions.push(tool);
      return (c.toolResults && c.toolResults[tool]) || `QA ${tool} result: synthetic-file.txt (1 KB)`;
    } };
    const toolbox = createToolboxes({
      boxes: [driveTools.box], driveTools, offered: (id) => id !== 'gdrive',
      mcpBoxes: () => [], mcpTools: () => new Map(), prefill: { budgetFor: () => null, rateFor: () => 0 }, requestScope,
      scope: requestScope, documentSources: { notice: () => '' }, workspace: () => ({ dir }),
    });

    const res = new EventEmitter();
    const out = { http: null, refusal: null, events: [], keepAlives: 0 };
    res.writableEnded = false;
    res.destroyed = false;
    res.writeHead = (status, headers) => { out.http = { status, headers }; };
    res.write = (line) => {
      if (line.startsWith(': ')) { out.keepAlives++; return true; }
      if (!line.startsWith('data: ') || !line.endsWith('\n\n')) throw new Error(`unexpected frame ${JSON.stringify(line)}`);
      out.events.push(JSON.parse(line.slice(6)));
      return true;
    };
    // As http.ServerResponse: finish and close come after end() returns; a destroyed response emits neither again.
    res.end = () => {
      if (res.writableEnded) return;
      res.writableEnded = true;
      if (res.destroyed) return;
      process.nextTick(() => { res.emit('finish'); res.emit('close'); });
    };
    const json = (r, status, payload) => { out.refusal = { status, payload }; r.end(); };

    const ctxSize = c.ctxSize || 32768;
    let leases = 0;
    const upstream = engine(c.rounds || [], log, res);
    const handler = createChatHandler({
      fs, path, crypto: fixedCrypto, fetch: upstream.fetch,
      reasoningEffort: require(path.join(server, 'reasoning-effort.cjs')), createToolExchange,
      rag: { filesContext: async () => null }, prefill: { recordSample() {} },
      reduceToolResult: require(path.join(server, 'tool-result-reduce.cjs')).reduceToolResult,
      HISTORY_CAP: 20, DEFAULT_PROVIDER_ID: 'default', DIARY_BASE: 'http://qa-diary.invalid', TOOL_RESULT_CAP: 8000,
      authService: { audit: (event, actor, subject, details) => store('audit', { event, actor, subject, details }), diaryEnabled: () => false },
      toolPolicy: { mode: (_user, _tool, write) => (write ? 'ask' : 'allow') },
      modelManager: {
        enabled: true,
        // The inference gate grants at once (llamacpp-manager.cjs acquireLease, inference-gate-client.cjs makeLease).
        acquireLease: async (model, options = {}) => {
          options.signal?.throwIfAborted();
          const lease = `qa-lease-${++leases}`;
          let released = false;
          store('modelManager.acquireLease', { model, lease });
          const withLease = (headers) => ({ ...(headers || {}), 'X-Noevia-Lease': lease });
          return {
            model, headers: () => ({ 'X-Noevia-Lease': lease }),
            // Gives the lease back once (inference-gate-client.cjs); Node calls it on both finish and close.
            release: async () => { if (released) return; released = true; store('lease.release', { lease }); },
            fetch: (baseFetch) => (url, init = {}) => baseFetch(url, { ...init, headers: withLease(init.headers) }),
          };
        },
        inferenceBusy: () => false,
        load: async (model) => { store('modelManager.load', { model }); return { ok: true }; },
        health: async () => ({ ok: true, body: { all_models_loaded: [{ model_name: MODEL, loaded: true, recipe_options: { ctx_size: ctxSize } }] } }),
      },
      requestScope, currentWorkspace: () => workspace, json, getProject: (id) => (project && id === project.id ? project : null),
      // providers.cjs providerHeaders: JSON, and a bearer key when the row has one other than 'local' (this one has none).
      getProvider: () => ({ id: 'default', baseUrl: ENGINE, label: 'QA engine' }),
      providerHeaders: (p, extra) => ({ 'Content-Type': 'application/json', ...(extra || {}), ...(p.apiKey && p.apiKey !== 'local' ? { Authorization: `Bearer ${p.apiKey}` } : {}) }),
      saveChats: (projectId, chats) => store('saveChats', { projectId, chats }), endpointApproved: () => true,
      diaryHeaders: () => ({}), diaryExtras: require(path.join(server, 'diary-extras.cjs')),
      autoRoles: () => (c.auto ? { fast: MODEL, smart: MODEL } : null), lastLoadedModel: () => (c.lastLoaded === undefined ? null : c.lastLoaded),
      // systemOneRouter.classifyWithDetails with no decision backend (system-one-router.cjs fallback branch).
      classifyFastOrSmart: async () => ({ role: c.auto || 'fast', routingDecision: { offered: [], scores: {}, selectedRole: null, effectiveRole: c.auto || 'fast',
        backend: 'legacy', model: null, calibrated: false, latencyMs: null, status: 'fallback', fallbackReason: 'disabled', fallbackCause: null } }), servedCatalogue: async () => [], modelsInstalled: async () => [], missingRoles: () => [], staleRolesError: () => null,
      visionProbe: async () => ({ supported: false, reason: 'none' }), visionDescriptions: new Map(), skillsIndexFor: () => [],
      chatSkillRouter: { select: async () => ({ loaded: [] }) }, chatToolRouter: { select: async (ids) => ({ ids, routed: false }) },
      ...toolbox, oauthServerIds: () => new Set(), accountReady: () => true,
      grantContext: async (raw) => grantFixture.context(raw, { holder: { kind: 'absent' } }),
      chatWideApproved: () => false,
      awaitApproval: async (request) => { const { abortSignal: _signal, ...asked } = request; log.approvals.push(asked); return c.decision || 'approve'; },
      // index.cjs: Drive writes are described before their card and again before dispatch (gdrive-tools describeTarget).
      writeTargetFor: (name, rawArgs, { user: who }) => (driveTools.names.has(name) ? driveTools.describeTarget(who, name, rawArgs) : null),
      recordUsage: (_ws, model, reported) => store('recordUsage', { model, reported }),
      recordToolUse: (_ws, name) => store('recordToolUse', { name }),
      chatTitles: c.titles ? { save: (listId, chatId, title, first) => store('chatTitles.save', { listId, chatId, title, first }), autoNamed: (listId, chatId, first) => { store('chatTitles.autoNamed', { listId, chatId, first }); return c.titles.autoNamed !== false; } } : null,
      sourceWaitMs: 0, streamIdleMs: c.streamIdleMs ?? 120000,
      ...(handoff ? { nativeHandoff: { requested: () => true, supervising: () => false } } : {}),
    });
    const body = { message: 'QA hello', projectId: project ? project.id : undefined, chatId: 'qa-chat-1', history: [], ...(c.body || {}) };
    await handler.handleChat({}, res, body, { user });
    await new Promise((resolve) => setImmediate(resolve)); // the response's finish/close and what they release
    if (out.keepAlives) throw new Error(`${c.name}: a keep-alive was written; the turn ran into the 5 s heartbeat`);
    if (handoff) {
      const last = out.events[out.events.length - 1];
      const prepared = last?.type === 'prepared' ? last : null;
      const refused = logged.map((l) => /^\[chat\] native executor not used for this turn: (.+)$/.exec(l)?.[1]).find(Boolean) || null;
      return { prepared, refused, before: prepared ? out.events.length - 1 : null, requests: prepared ? log.requests.length : null };
    }
    // The script is exact: every round it names is asked for, and no other.
    if (upstream.used() !== (c.rounds || []).length) throw new Error(`${c.name}: the turn made ${upstream.used()} engine requests, the script has ${(c.rounds || []).length}`);
    const record = createTurnRecord({ now: () => FIXED_NOW });
    for (const event of out.events) record.observe(event);
    return {
      name: c.name,
      about: c.about,
      input: { body, project, ctxSize, auto: c.auto || null, decision: c.decision || null, titles: c.titles || null, streamIdleMs: c.streamIdleMs ?? null, appendOutcome: c.appendOutcome || null, rounds: c.rounds || [] },
      http: out.http, refusal: out.refusal, events: out.events,
      requests: log.requests, approvals: log.approvals, executions: log.executions, stores: log.stores,
      files: listFiles(dir),
      record: { assistantEntry: record.assistantEntry(), partialEntry: record.partialEntry() },
    };
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

// ── The matrix ──────────────────────────────────────────────────────────────────────────────
const chunk = (delta, extra = {}) => ({ choices: [{ index: 0, delta, finish_reason: null }], ...extra });
const finish = (reason) => ({ choices: [{ index: 0, delta: {}, finish_reason: reason }] });
const usage = (prompt, completion, perSecond) => ({ choices: [], usage: { prompt_tokens: prompt, completion_tokens: completion, total_tokens: prompt + completion }, ...(perSecond ? { timings: { predicted_per_second: perSecond } } : {}) });
const toolCall = (name, args, id = 'qa-call-1', index = 0) => chunk({ tool_calls: [{ index, id, type: 'function', function: { name, arguments: args } }] });
const summary = (text) => ({ json: { choices: [{ message: { role: 'assistant', content: text }, finish_reason: 'stop' }] } });
const answer = (text) => ({ frames: [chunk({ content: text }), finish('stop'), usage(40, 4, 20), { done: true }] });
const longText = (label, n) => `${label} ${'QA filler sentence about synthetic data. '.repeat(n)}`;
const longHistory = (exchanges, n) => Array.from({ length: exchanges }, (_, i) => [
  { role: 'user', content: longText(`QA question ${i + 1}.`, n) },
  { role: 'assistant', content: longText(`QA answer ${i + 1}.`, n) },
]).flat();

const CASES = [
  { name: 'plain', about: 'A plain reply in two deltas, with usage and timings, ended by [DONE].',
    rounds: [{ frames: [chunk({ role: 'assistant', content: '' }), chunk({ content: 'Hello from ' }), chunk({ content: 'QA.' }), finish('stop'), usage(52, 5, 25), { done: true }] }] },
  { name: 'plain_split_frames', about: 'Frames cut mid-line across chunks, a keep-alive comment and a non-JSON line are reassembled or skipped.',
    rounds: [{ frames: [': ping\n\n', { split: chunk({ content: 'Split ' }) }, 'data: not-json\n\n', { split: chunk({ content: 'frame.' }) }, finish('stop'), { done: true }] }] },
  { name: 'astral_split_bytes', about: 'An answer with astral characters, one of them cut between two chunks inside its UTF-8 bytes.',
    rounds: [{ frames: [{ splitBytes: chunk({ content: 'QA \u{1F600} smile and \u{1D11E} clef' }) }, finish('stop'), { done: true }] }] },
  { name: 'no_project', about: 'A free chat: no project, the loaded model answers.', project: null, lastLoaded: MODEL,
    body: { projectId: undefined, spaceId: 'free' }, rounds: [answer('Free chat reply.')] },
  { name: 'reasoning', about: 'reasoning_content and reasoning deltas before the answer.',
    rounds: [{ frames: [chunk({ reasoning_content: 'QA thinking one. ' }), chunk({ reasoning: 'QA thinking two.' }), chunk({ content: 'Reasoned answer.' }), finish('stop'), usage(60, 12, 30), { done: true }] }] },
  { name: 'reasoning_only', about: 'Reasoning with no answer: the fixed note is sent as a delta.',
    rounds: [{ frames: [chunk({ reasoning_content: 'QA thinking with no answer.' }), finish('stop'), { done: true }] },
      // The thinking cap asks once more for the answer alone (thinking-budget.cjs #1477); it writes none either.
      { frames: [finish('stop'), { done: true }] }] },
  { name: 'stopped_mid_stream', about: 'The client leaves after two deltas: nothing more is sent and the reply is kept as stopped.',
    rounds: [{ frames: [chunk({ content: 'Partial ' }), chunk({ content: 'answer' }), { leave: true }, chunk({ content: ' never sent' })] }] },
  { name: 'stopped_before_output', about: 'The client leaves before any output: the Stopped placeholder is kept.',
    rounds: [{ frames: [{ leave: true }] }] },
  { name: 'stopped_store_refuses', about: 'The client leaves, the transcript does not take the reply: the partial file keeps it.',
    appendOutcome: 'no-user-message', rounds: [{ frames: [chunk({ content: 'Kept only as a partial' }), { leave: true }] }] },
  { name: 'engine_http_500', about: 'The engine answers 500 before any output.',
    rounds: [{ status: 500, body: JSON.stringify({ error: { message: 'QA engine failure', type: 'server_error' } }) }] },
  { name: 'engine_unreachable', about: 'The engine request throws.', rounds: [{ throws: 'connect ECONNREFUSED qa-engine.invalid' }] },
  { name: 'engine_stream_error', about: 'An error frame after some answer text.',
    rounds: [{ frames: [chunk({ content: 'Some text ' }), { error: { message: 'QA mid-stream failure', code: 500 } }, chunk({ content: 'after' })] }] },
  { name: 'engine_stalls', about: 'The stream stops sending after its first output: ended by the idle limit.', streamIdleMs: 100,
    rounds: [{ frames: [chunk({ content: 'Before the stall' }), { stall: true }] }] },
  { name: 'empty_stream_fallback', about: 'A stream with nothing in it: one non-streaming retry answers.',
    rounds: [{ frames: [{ done: true }] }, { json: { choices: [{ message: { role: 'assistant', content: 'Fallback answer.' }, finish_reason: 'stop' }], usage: { prompt_tokens: 30, completion_tokens: 3, total_tokens: 33 } } }] },
  { name: 'length_cut', about: 'finish_reason length: the budget warning, the reply still completes.',
    rounds: [{ frames: [chunk({ content: 'Cut off at the' }), finish('length'), usage(50, 100), { done: true }] }] },
  { name: 'title', about: 'A first completed reply with titles wired: a title request after done, a title event and a save.', titles: { autoNamed: true },
    rounds: [answer('Here is the train timetable.'), { json: { choices: [{ message: { role: 'assistant', content: 'QA Train Timetable' }, finish_reason: 'stop' }] } }] },
  { name: 'title_renamed', about: 'A later reply whose chat was renamed: the store says not auto-named, so no title request.', titles: { autoNamed: false },
    body: { history: [{ role: 'user', content: 'QA first' }, { role: 'assistant', content: 'QA first reply' }] }, rounds: [answer('Second reply.')] },
  { name: 'title_still_derived', about: 'A later reply whose chat still has its derived title (the first reply was stopped): a title is asked for.', titles: { autoNamed: true },
    body: { history: [{ role: 'user', content: 'QA first' }, { role: 'assistant', content: 'QA first reply' }] },
    rounds: [answer('Second reply.'), { json: { choices: [{ message: { role: 'assistant', content: '"Title: QA Second Topic."' }, finish_reason: 'stop' }] } }] },
  { name: 'title_failed', about: 'The title request fails: the reply is unchanged and nothing is saved.', titles: { autoNamed: true },
    rounds: [answer('A reply.'), { status: 503, body: 'busy' }] },
  { name: 'tool_read', about: 'A read tool runs without approval, its result goes back, the next round answers.',
    rounds: [{ frames: [chunk({ content: 'Let me look. ' }), toolCall('drive_read_file', '{"fileId":"qa-file-1"}'), finish('tool_calls'), { done: true }] }, answer('The file says hello.')] },
  { name: 'tool_streamed_args', about: 'A tool call whose name and arguments arrive in fragments, with no id.',
    rounds: [{ frames: [chunk({ tool_calls: [{ index: 0, function: { name: 'drive_' } }] }), chunk({ tool_calls: [{ index: 0, function: { name: 'read_file', arguments: '{"file' } }] }), chunk({ tool_calls: [{ index: 0, function: { arguments: 'Id":"qa-file-2"}' } }] }), finish('tool_calls')] }, answer('Read it.')] },
  { name: 'tool_approved', about: 'A write asks, is approved and runs; the next round answers.', decision: 'approve',
    rounds: [{ frames: [toolCall('drive_trash_file', '{"fileId":"qa-file-3"}'), finish('tool_calls')] }, answer('Moved it to the trash.')] },
  { name: 'tool_declined', about: 'A write asks and is declined: the reply ends with the paused note, no further round.', decision: 'deny',
    rounds: [{ frames: [toolCall('drive_trash_file', '{"fileId":"qa-file-4"}'), finish('tool_calls')] }] },
  { name: 'tool_approval_timeout', about: 'A write asks and the card times out.', decision: 'timeout',
    rounds: [{ frames: [toolCall('drive_trash_file', '{"fileId":"qa-file-5"}'), finish('tool_calls')] }, answer('I did not trash it.')] },
  { name: 'tool_write_target_refused', about: 'A Drive write whose file id is not one: refused before any card, nothing asked.',
    rounds: [{ frames: [toolCall('drive_trash_file', '{"fileId":"not a drive id!"}'), finish('tool_calls')] }, answer('That id was not valid.')] },
  { name: 'tool_unknown', about: 'A call of a tool the turn never offered.',
    rounds: [{ frames: [toolCall('qa_not_offered', '{}'), finish('tool_calls')] }, answer('That tool does not exist.')] },
  { name: 'final_pass', about: 'Three rounds spent on tool calls: one more round with no tools asks for the answer.',
    rounds: [
      { frames: [toolCall('drive_read_file', '{"fileId":"qa-a"}', 'qa-r1'), finish('tool_calls')] },
      { frames: [toolCall('drive_read_file', '{"fileId":"qa-b"}', 'qa-r2'), finish('tool_calls')] },
      { frames: [toolCall('drive_read_file', '{"fileId":"qa-c"}', 'qa-r3'), finish('tool_calls')] },
      answer('Final answer from the three reads.')] },
  { name: 'final_pass_no_answer', about: 'The final-answer round writes no answer either: the fallback text with the gathered links.',
    toolResults: { drive_read_file: 'QA read result with a link https://qa.example.invalid/doc-1' },
    rounds: [
      { frames: [toolCall('drive_read_file', '{"fileId":"qa-a"}', 'qa-r1'), finish('tool_calls')] },
      { frames: [chunk({ reasoning_content: 'Only thinking after the result.' }), finish('stop')] },
      { frames: [finish('stop')] },
      { frames: [chunk({ reasoning_content: 'Still only thinking.' }), finish('stop')] },
      { frames: [finish('stop')] }] },
  { name: 'fallback_fails_then_final_pass', about: 'A tool round, then an empty stream whose non-streaming retry fails: the error is not the end, a final-answer round follows.',
    rounds: [
      { frames: [toolCall('drive_read_file', '{"fileId":"qa-a"}', 'qa-r1'), finish('tool_calls')] },
      { frames: [{ done: true }] },
      { status: 500, body: JSON.stringify({ error: { message: 'QA fallback failure' } }) },
      answer('Answer from the final pass.')] },
  { name: 'auto_fast', about: 'Automatic routing picks Fast: the route in meta, thinking off, effort low.', auto: 'fast',
    project: { routing: 'auto', model: undefined }, rounds: [answer('Fast reply.')] },
  { name: 'context_compacted', about: 'History over the context budget: older turns are summarised before the reply.', ctxSize: 4096,
    body: { history: longHistory(6, 20) },
    rounds: [summary('QA summary of the earlier turns, part one.'), summary('QA summary of the earlier turns, part two.'), answer('Answer after compaction.')] },
  { name: 'context_too_full', about: 'The current message alone does not fit: an error event, no engine request.', ctxSize: 2048,
    body: { message: longText('QA huge message.', 400) }, rounds: [] },
  { name: 'refused_no_message', about: 'A request with no message is refused before the stream opens.', body: { message: '' }, rounds: [] },
  { name: 'refused_unknown_project', about: 'A project id that does not exist.', body: { projectId: 'qa-missing' }, rounds: [] },
  { name: 'refused_no_model', about: 'A free chat with no model loaded.', project: null, body: { projectId: undefined }, rounds: [] },
];

(async () => {
  const cases = [];
  // The hand-off is recorded when the core has it (track R8, noevia-core#362): what the native executor gets.
  const withHandoff = fs.existsSync(path.join(server, 'rust-chat-native.cjs'));
  for (const c of CASES) {
    const turn = await runTurn(c);
    if (withHandoff && turn.http) turn.handoff = await runTurn(c, { handoff: true });
    cases.push(turn);
  }
  const tools = {};
  for (const c of cases) for (const r of c.requests) {
    if (!Array.isArray(r.body?.tools)) continue;
    const key = crypto.createHash('sha256').update(JSON.stringify(r.body.tools)).digest('hex').slice(0, 16);
    tools[key] = r.body.tools;
    r.body.tools = { $tools: key };
  }
  const fixture = {
    version: 1,
    generator: 'crates/chat-turn/tests/record_turns.cjs (noevia-core server/chat.cjs handleChat)',
    fixedNow: new RealDate(FIXED_NOW).toISOString(),
    sources: SOURCES,
    tools,
    cases,
  };
  const text = `${JSON.stringify(fixture, null, 1)}\n`;
  const check = process.argv.indexOf('--check');
  if (check > 0) {
    const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
    if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
    process.stderr.write(`ok: ${cases.length} turns recorded as committed\n`);
  } else process.stdout.write(text);
  // A turn leaves no timers behind; exit even if a module keeps a handle (the partial timer is unref'd).
  setImmediate(() => process.exit(0));
})().catch((error) => { process.stderr.write(`${error?.stack || error}\n`); process.exit(1); });
