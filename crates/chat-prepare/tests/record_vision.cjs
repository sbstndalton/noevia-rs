#!/usr/bin/env node
'use strict';
// Recorded image parts and vision pass of a turn for crates/chat-prepare (slice P2c): the contract src/vision.rs is
// replayed against (with src/lib.rs prepare() for the wire before it).
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-prepare/tests/record_vision.cjs > crates/chat-prepare/tests/fixtures/node-vision.v1.json
//   ... node crates/chat-prepare/tests/record_vision.cjs --check crates/chat-prepare/tests/fixtures/node-vision.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// The checkout needs server/wasm/dav-parse.wasm (frameUntrusted and the egress rules are the Rust ports in it); nothing
// from node_modules, no network. Every expectation comes from noevia-core's own code; the inputs are synthetic and
// QA-named (tiny synthetic PNG/JPEG/GIF/WebP bytes, hand-made edges plus a seeded PRNG), passed through JSON first so
// Node reads exactly what is recorded:
//   turns  chat.cjs createChatHandler().handleChat, whole turns with the front's native executor asking for them
//          (nativeHandoff, the `skills` capability), multimodal retrieval off, no tools, no Inference machines, no
//          routing modes, chat framing off. The project's image sources are written to a per-turn asset directory
//          (`images`: asset id -> base64 bytes; a file outside it for the path cases), and fs.readFileSync is
//          logged for every path under the turn's directory (`reads`: the path relative to it and whether it read).
//          From the meta event to the `generating` status the seams are stubs that answer per case and log their
//          calls in order (`calls`): autoRoles, getProvider, providerHeaders, modelManager.acquireLease (its lease
//          logs release; its fetch is the plain one), fetch (the describe request: url, headers, body text, answered
//          per case: a description, a status, a body that is not JSON or too long, a throw, a hang until the stage
//          deadline, the client leaving), visionProbe (its arguments) and modelManager.recordEvidence. The handler's
//          visionDescriptions Map is shared by the turns of a cache group; its keys are recorded after each such turn.
//          Recorded: those calls, the status and warning events after the meta event, the `[assets]` and `[vision]`
//          lines, and the `prepared` frame's messages and skills.imageWarning; or, for a turn the executor refuses
//          (another provider, a Diary project), the refusal and the messages of the model request; or what
//          handleChat threw.
// Each source file's sha256 is recorded (and chat.cjs's ported blocks), so a changed Node file needs a fresh recording.
// Node's console output goes to stderr; stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { EventEmitter } = require('node:events');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
const logged = [];
for (const k of ['log', 'info', 'warn', 'error', 'debug']) {
  console[k] = (...a) => { const line = a.map(String).join(' '); logged.push(line); process.stderr.write(`${line}\n`); };
}
const READ_FILES = ['server/chat.cjs', 'server/dav-parse.lock', 'server/wasm/dav-parse.wasm'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex');
function sourcesRead() {
  const server = `${path.join(root, 'server')}${path.sep}`;
  const loaded = Object.keys(require.cache).filter((k) => k.startsWith(server)).map((k) => path.relative(root, k).split(path.sep).join('/'));
  if (loaded.some((k) => k.includes('node_modules/'))) throw new Error('a module from node_modules was loaded: the header says nothing is');
  return Object.fromEntries([...new Set([...READ_FILES, ...loaded])].sort().map((f) => [f, sha(f)]));
}
const req = (f) => require(path.join(root, 'server', f));
const egress = req('provider-egress.cjs');
const diaryExtras = req('diary-extras.cjs');
const { createChatHandler } = req('chat.cjs');

// ── chat.cjs's ported lines ─────────────────────────────────────────────────────────────────
const chatText = fs.readFileSync(path.join(root, 'server/chat.cjs'), 'utf8');
const blocks = {};
function cut(name, start, end) {
  const once = (needle) => {
    const at = chatText.indexOf(needle);
    if (at < 0 || chatText.indexOf(needle, at + 1) >= 0) throw new Error(`chat.cjs holds ${JSON.stringify(needle.slice(0, 60))} ${at < 0 ? 'nowhere' : 'twice'}`);
    return at;
  };
  const from = once(start), to = once(end) + end.length;
  blocks[name] = { lines: [chatText.slice(0, from).split('\n').length, chatText.slice(0, to).split('\n').length - 1],
    sha256: crypto.createHash('sha256').update(chatText.slice(from, to)).digest('hex') };
}
cut('codeRouteNote', 'const CODE_ROUTE_NOTE = ', "name the concrete mechanism in one sentence.';\n");
cut('describe', '    const VISION_DESCRIBE_TIMEOUT_MS = 180000;\n', '      } finally { await stageLease?.release(); }\n    };\n');
cut('images', '    const projectImages = project ? (project.assets || []) : [];\n',
  '          console.warn(`[assets] ${asset.id} is listed on project ${project.id} but its bytes are missing`);\n        }\n      }\n    }\n');
cut('pass', '    let visionWarning = missingImages.length ?', "          if (!sys) wire = [{ role: 'system', content: blind }, ...wire];\n        }\n      }\n    }\n");
cut('replyWarning', "    send({ type: 'status', id: 'generating', text: 'Generating response…' });\n", "    if (replyWarning()) send({ type: 'warning', text: replyWarning() });\n");
cut('frameImageWarning', 'revokedInHistory: [...revokedInHistory], imageWarning, coverageWarning', 'revokedInHistory: [...revokedInHistory], imageWarning, coverageWarning');

// ── Deterministic inputs ────────────────────────────────────────────────────────────────────
let seed = 0x2026_1113 ^ 0x2c;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const pick = (list) => list[Math.floor(rand() * list.length)];
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

// Synthetic image bytes (base64): 1x1 PNG, JPEG, GIF and WebP, an empty file, and a larger PNG-signed PRNG blob.
const PNG = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP4//8/AwAI/AL+p9akwQAAAABJRU5ErkJggg==';
const JPEG = '/9j/4AAQSkZJRgABAQEASABIAAD/2wBDAP//////////////////////////////////////////////////////////////////////////////////////wgALCAABAAEBAREA/8QAFBABAAAAAAAAAAAAAAAAAAAAAP/aAAgBAQABPxA=';
const GIF = 'R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7';
const WEBP = 'UklGRhoAAABXRUJQVlA4TA0AAAAvAAAAEAcQERGIiP4HAA==';
const EMPTY = '';
const LARGE = Buffer.concat([Buffer.from('89504e470d0a1a0a', 'hex'), Buffer.from(Array.from({ length: 12001 }, () => int(0, 255)))]).toString('base64');
const TWO = Buffer.from([0xff, 0xfe]).toString('base64');

const PROVIDER = 'http://qa-default.invalid';
const PROVIDERS = {
  default: { id: 'default', label: 'QA local', baseUrl: `${PROVIDER}/v1` },
  'qa-cloud': { id: 'qa-cloud', label: 'QA Cloud', baseUrl: 'https://qa-cloud.invalid/v1', external: true, shared: true },
  'qa-cloud-unlabelled': { id: 'qa-cloud-unlabelled', baseUrl: 'https://qa-cloud-2.invalid/v1', external: true, shared: true },
  'qa-second': { id: 'qa-second', label: 'QA Second', baseUrl: 'http://127.0.0.1:18999/v1', shared: true },
};
const BLIND = { supported: false, reason: 'QA engine reports no image input.' };
const SEES = { supported: true, reason: null };
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-chat-vision-'));
process.on('exit', () => fs.rmSync(tmp, { recursive: true, force: true }));
const groups = new Map(); // cache group -> the visionDescriptions Map its turns share

let current = null; // the running turn
const seam = (entry) => { if (current.phase === 'vision') current.calls.push(entry); };
const abortError = (message) => Object.assign(new Error(message), { name: 'AbortError' });
const waitAbort = (signal) => new Promise((_, reject) => {
  if (signal.aborted) { reject(signal.reason); return; }
  signal.addEventListener('abort', () => reject(signal.reason), { once: true });
});

// The describe answer of a case (`describe`), as a fetch Response (or a throw).
async function describeAnswer(spec, init) {
  const d = spec.describe || { text: 'QA description: a white square with one dot.' };
  if (d.delayMs) await new Promise((resolve) => setTimeout(resolve, d.delayMs));
  if (d.hang) return waitAbort(init.signal);
  if (d.leave) { current.res.emit('close'); throw abortError('QA client left'); }
  if (d.throws) throw new Error(d.throws);
  if (d.status) return new Response(JSON.stringify({ error: { message: 'QA vision failure' } }), { status: d.status });
  if ('raw' in d) return new Response(d.raw, { status: 200 });
  if (d.big) return new Response(bigBody(d.big), { status: 200 });
  if ('json' in d) return new Response(JSON.stringify(d.json), { status: 200 });
  return new Response(JSON.stringify({ choices: [{ index: 0, finish_reason: d.finish || 'stop', message: { role: 'assistant', content: d.text } }] }), { status: 200 });
}
// A reply of exactly 4 MiB ('cap') or one byte more ('over'): the description padded with spaces after the JSON.
const CAP = 4 * 1024 * 1024;
function bigBody(kind) {
  const json = JSON.stringify({ choices: [{ finish_reason: 'stop', message: { content: 'QA description of a big reply.' } }] });
  return json + ' '.repeat(CAP - json.length + (kind === 'over' ? 1 : 0));
}

function handlerFor(spec, turnDir, assetsDir) {
  const S = () => current.spec;
  const fetch = async (url, init) => {
    if (current.phase === 'vision') {
      seam(['fetch', url, init.headers, init.body]);
      return describeAnswer(S(), init);
    }
    current.requests.push(JSON.parse(init.body));
    return { ok: true, body: (async function* () { yield Buffer.from(`data: ${JSON.stringify({ choices: [{ delta: { content: 'QA ok' } }] })}\n\n`); })() };
  };
  // fs: every read under the turn's directory is logged (the asset reads); the rest is node:fs.
  const fsLogged = Object.assign(Object.create(fs), {
    readFileSync: (p, ...rest) => {
      const rel = typeof p === 'string' && (p === turnDir || p.startsWith(`${turnDir}${path.sep}`)) ? path.relative(turnDir, p).split(path.sep).join('/') : null;
      try {
        const out = fs.readFileSync(p, ...rest);
        if (rel !== null) current.reads.push([rel, true]);
        return out;
      } catch (e) {
        if (rel !== null) current.reads.push([rel, false]);
        throw e;
      }
    },
  });
  const lease = async (model, options = {}) => {
    seam(['acquireLease', model]);
    const answer = S().leaseAnswer;
    // Only the Vision role's lease is scripted; the reply's own (after the recording) is granted.
    if (current.phase === 'vision' && answer) {
      if (answer === 'code') throw Object.assign(new Error('QA gate refusal'), { code: 'qa_busy' });
      if (answer === 'status') throw Object.assign(new Error('QA gate refusal'), { status: 503 });
      if (answer === 'bare') throw new Error('QA gate refusal');
      if (answer === 'hang') return waitAbort(options.signal);
      if (answer === 'leave') { current.res.emit('close'); throw abortError('QA client left'); }
    }
    return { model, headers: () => ({}), fetch: (base) => base, release: async () => { seam(['release']); } };
  };
  const manager = { enabled: true,
    health: async () => ({ ok: true, body: { all_models_loaded: [{ model_name: 'qa-model', loaded: true, recipe_options: { ctx_size: 1048576 } }] } }),
    ...(spec.lease === false ? {} : { acquireLease: lease }),
    ...(spec.evidence === false ? {} : { recordEvidence: async (model, evidence) => { seam(['recordEvidence', model, evidence]); } }) };
  return createChatHandler({
    modelManager: manager, reasoningEffort: req('reasoning-effort.cjs'), authService: { audit() {}, diaryEnabled: () => true }, crypto, path, fs: fsLogged, fetch,
    HISTORY_CAP: 1000, DEFAULT_PROVIDER_ID: 'default', createToolExchange: () => async () => 'unused',
    currentWorkspace: () => ({ userId: S().user, dir: turnDir, assetDir: () => assetsDir }),
    getProject: (id) => (S().project && id === S().project.id ? structuredClone(S().project) : null), skillsIndexFor: () => [],
    // After the meta event (the describe provider): the case's `describeRow` when it has one.
    getProvider: (id) => {
      seam(['getProvider', id]);
      if (current.phase === 'vision' && 'describeRow' in S()) return structuredClone(S().describeRow);
      return structuredClone(PROVIDERS[id] || null);
    },
    // providers.cjs providerHeaders over the QA rows: the row's id, and the case's extra headers for it.
    providerHeaders: (p) => {
      seam(['providerHeaders', p ? p.id ?? null : null]);
      return { 'X-QA-Provider': String(p?.id), ...((S().headers || {})[p?.id] || {}) };
    },
    autoRoles: () => { seam(['autoRoles']); return structuredClone(S().roles ?? null); },
    visionDescriptions: groups.get(spec.group) || new Map(),
    visionProbe: async (baseUrl, headers, model) => { seam(['visionProbe', baseUrl, headers, model]); return structuredClone(S().vision ?? BLIND); },
    visionStageTimeoutMs: spec.stageMs ?? 120000,
    chatSkillRouter: { select: async () => ({ loaded: [] }) }, oauthServerIds: () => new Set(), accountReady: () => true, mcpOAuth: { connected: () => false },
    chatToolRouter: { select: async (ids) => ({ ids, routed: false }) }, DEFAULT_TOOLBOXES: [], CONNECTOR_BOXES: new Set(), connectedBoxes: () => [],
    toolPolicy: { mode: () => 'allow' }, requestScope: { getStore: () => ({}) }, resolveTools: () => ({ tools: [], dropped: [] }), isWriteTool: () => false,
    rag: { filesContext: async () => null }, prefill: { recordSample() {} }, reduceToolResult: () => ({ text: 'reduced' }), diaryExtras,
    DIARY_BASE: 'http://qa-diary.invalid', TOOL_RESULT_CAP: 8000, json: (res, status, body) => { current.refusedJson = { status, body }; },
    saveChats() {}, endpointApproved: () => true, diaryHeaders: () => ({}), lastLoadedModel: () => 'qa-model',
    classifyFastOrSmart: async () => structuredClone(S().classification ?? 'fast'),
    servedCatalogue: async () => [], modelsInstalled: async () => [], missingRoles: () => [], staleRolesError: () => null, allToolboxes: () => [],
    chatWideApproved: () => false, recordUsage() {}, recordToolUse() {}, executeToolCall: async () => 'unused', awaitApproval: async () => 'deny',
    codeTasksFor: () => [], sourceJobs: { settle: async () => ({ notReady: [], aborted: false }) },
    skillHistory: { scrub: ({ messages }) => ({ messages, removed: [] }), record() {} },
    multimodalEnabled: () => false, chatFramingEnabled: () => false,
    nativeHandoff: { requested: () => true, capabilities: () => new Set(['skills']), supervising: () => false },
  });
}

const turns = [];
const P = (extra = {}) => ({ id: 'qa-project-1', name: 'QA Project', routing: 'manual', model: 'qa-model', ...extra });
const asset = (id, name, mime = 'image/png') => ({ id, name, mime });
// `images`: the asset files (id -> base64); `outside`: base64 of a file next to the asset directory.
async function addTurn(name, input) {
  const spec = viaJson({ message: 'QA what is in the chart?', user: 'qa-user', ...input });
  if (spec.group && !groups.has(spec.group)) groups.set(spec.group, new Map());
  const cache = spec.group ? groups.get(spec.group) : null;
  if (cache && spec.expire) for (const v of cache.values()) v.until = 0;
  const turnDir = fs.mkdtempSync(path.join(tmp, 'qa-'));
  const assetsDir = path.join(turnDir, 'assets');
  fs.mkdirSync(assetsDir);
  for (const [id, b64] of Object.entries(spec.images || {})) fs.writeFileSync(path.join(assetsDir, id), Buffer.from(b64, 'base64'));
  if (spec.outside !== undefined) fs.writeFileSync(path.join(turnDir, 'qa-outside.png'), Buffer.from(spec.outside, 'base64'));
  process.env.NOEVIA_CONTRACT_FIXED_NOW = '2026-11-13T12:00:00Z';
  process.env.TZ = 'UTC';
  const raw = [];
  const res = new EventEmitter();
  res.writeHead = () => {}; res.setHeader = () => {};
  res.write = (line) => {
    const s = String(line);
    raw.push(s);
    // Forward only: a Node-run turn writes a later meta event with its reply.
    if (current.phase === 'before' && s.includes('"type":"meta"')) current.phase = 'vision';
    if (current.phase === 'vision' && s.includes('"id":"generating"')) current.phase = 'after';
    return true;
  };
  res.end = () => { res.writableEnded = true; res.emit('finish'); };
  current = { spec, res, phase: 'before', calls: [], reads: [], requests: [], refusedJson: null };
  const { handleChat } = handlerFor(spec, turnDir, assetsDir);
  const body = { message: spec.message, ...(spec.project && !spec.free ? { projectId: spec.project.id } : {}), ...(spec.body || {}) };
  const from = logged.length;
  let thrown = null;
  try { await handleChat({ headers: {} }, res, body, { user: { id: spec.user } }); } catch (e) { thrown = e.constructor.name; process.stderr.write(`QA ${name} threw: ${e.stack}\n`); }
  await new Promise((resolve) => setImmediate(resolve));
  const events = raw.join('').split('\n\n').filter((x) => x.startsWith('data: ')).map((x) => JSON.parse(x.slice(6)));
  const metaAt = events.findIndex((e) => e.type === 'meta');
  const genAt = events.findIndex((e) => e.type === 'status' && e.id === 'generating');
  const window = metaAt < 0 ? [] : events.slice(metaAt + 1, genAt < 0 ? events.length : genAt + 2);
  const seen = window.filter((e) => e.type === 'status' || e.type === 'warning').map((e) => (e.type === 'warning' ? { type: 'warning', text: e.text } : { type: 'status', id: e.id, text: e.text }));
  const prepared = events.find((e) => e.type === 'prepared') || null;
  const lines = logged.slice(from).filter((l) => l.startsWith('[assets]') || l.startsWith('[vision]'));
  const refused = logged.slice(from).map((l) => /^\[chat\] native executor not used for this turn: (.+)$/.exec(l)?.[1]).find(Boolean) || null;
  const reply = current.requests.find((r) => r.stream === true) || null;
  const outcome = thrown ? { throws: thrown }
    : current.refusedJson ? { json: current.refusedJson }
    : prepared ? { frame: { messages: prepared.messages, imageWarning: prepared.skills ? prepared.skills.imageWarning : null } }
    : { refused, messages: reply ? reply.messages : null };
  if (!thrown && !prepared && !current.refusedJson && !spec.refusedOk) throw new Error(`${name}: no frame (refused: ${refused}); events: ${raw.join('').slice(0, 600)}`);
  const describeRow = spec.describeRow;
  turns.push({ name, ...(spec.group ? { group: spec.group } : {}), spec,
    describeExternal: describeRow && typeof describeRow === 'object' ? egress.isExternalProvider(describeRow) : null,
    body, reads: current.reads, calls: current.calls, events: seen, lines, ...outcome,
    ...(cache ? { cache: [...cache.keys()] } : {}) });
}

(async () => {
  const IMG1 = { images: { 'img-qa-1': PNG } };
  const ONE = { project: P({ assets: [asset('img-qa-1', 'qa-chart.png')] }), ...IMG1 };
  const VR = { roles: { vision: 'qa-vision' } };
  // ── No images ───────────────────────────────────────────────────────────────────────────
  await addTurn('free_chat', { body: { spaceId: 'free', chatId: 'qa-chat-1' } });
  await addTurn('project_no_assets', { project: P() });
  await addTurn('assets_empty', { project: P({ assets: [] }), vision: SEES, ...VR });
  for (const [label, assets] of [['object', {}], ['number', 5], ['false', false], ['null', null], ['empty_string', '']]) {
    await addTurn(`assets_${label}`, { project: P({ assets }), vision: SEES });
  }
  await addTurn('assets_string_throws', { project: P({ assets: 'qa' }) });
  await addTurn('assets_length_object_throws', { project: P({ assets: { length: 1 } }) });
  await addTurn('assets_object_named_throws', { project: P({ assets: {} }), body: { turnImageIds: ['img-qa-1'] } });

  // ── Images the answering model can see ──────────────────────────────────────────────────
  await addTurn('seen_png', { ...ONE, vision: SEES });
  await addTurn('seen_jpeg', { project: P({ assets: [asset('img-qa-1', 'qa-photo.jpg', 'image/jpeg')] }), images: { 'img-qa-1': JPEG }, vision: SEES });
  await addTurn('seen_gif_webp', { project: P({ assets: [asset('img-qa-1', 'qa-anim.gif', 'image/gif'), asset('img-qa-2', 'qa-pic.webp', 'image/webp')] }),
    images: { 'img-qa-1': GIF, 'img-qa-2': WEBP }, vision: SEES });
  await addTurn('seen_empty_file', { project: P({ assets: [asset('img-qa-1', 'qa-empty.png')] }), images: { 'img-qa-1': EMPTY }, vision: SEES });
  await addTurn('seen_large', { project: P({ assets: [asset('img-qa-1', 'qa-large.png')] }), images: { 'img-qa-1': LARGE }, vision: SEES });
  await addTurn('seen_three_one_missing', { project: P({ assets: [asset('img-qa-1', 'qa-a.png'), asset('img-qa-2', 'qa-b.jpg', 'image/jpeg'), asset('img-qa-3', 'qa-c.png')] }),
    images: { 'img-qa-1': PNG, 'img-qa-3': TWO }, vision: SEES });
  await addTurn('seen_truthy_supported', { ...ONE, vision: { supported: 1, reason: 'QA' } });
  await addTurn('seen_text_supported', { ...ONE, vision: { supported: 'false' } });
  await addTurn('seen_no_evidence_hook', { ...ONE, vision: SEES, evidence: false });
  await addTurn('seen_history', { ...ONE, vision: SEES, body: { history: [{ role: 'user', content: 'QA earlier question' }, { role: 'assistant', content: 'QA earlier answer' }] } });
  await addTurn('seen_duplicate_ids', { project: P({ assets: [asset('img-qa-1', 'qa-a.png'), asset('img-qa-1', 'qa-a again.png')] }), ...IMG1, vision: SEES });
  for (const [label, mime] of [['undefined', undefined], ['number', 5], ['svg', 'image/svg+xml'], ['list', ['image/png', 'x']], ['empty', ''], ['null', null]]) {
    const a = asset('img-qa-1', `qa-mime-${label}.png`);
    if (mime === undefined) delete a.mime; else a.mime = mime;
    await addTurn(`mime_${label}`, { project: P({ assets: [a] }), ...IMG1, vision: SEES });
  }
  await addTurn('mime_throws_loaded_and_missing', { project: P({ assets: [{ id: 'img-qa-1', name: 'qa-mime.png', mime: { toString: 1 } }, asset('img-qa-2', 'qa-ok.png')] }),
    images: { 'img-qa-1': PNG, 'img-qa-2': PNG }, vision: SEES });

  // ── Missing stored files, asset and id kinds ────────────────────────────────────────────
  await addTurn('missing_all', { project: P({ assets: [asset('img-qa-1', 'qa-gone.png'), asset('img-qa-2', 'qa-gone 2.png')] }), vision: SEES, ...VR });
  await addTurn('missing_one_blind', { project: P({ assets: [asset('img-qa-1', 'qa-a.png'), asset('img-qa-2', 'qa-gone.png')] }), ...IMG1 });
  for (const [label, id] of [['number', 5], ['null', null], ['empty', ''], ['dot', '.'], ['dotdot', '..'], ['nul', 'img-qa-1\u0000'], ['object', { qa: 1 }]]) {
    await addTurn(`id_${label}`, { project: P({ assets: [{ id, name: `qa-id-${label}.png`, mime: 'image/png' }, asset('img-qa-1', 'qa-ok.png')] }), ...IMG1, vision: SEES });
  }
  await addTurn('id_missing', { project: P({ assets: [{ name: 'qa-no-id.png', mime: 'image/png' }] }), vision: SEES });
  await addTurn('id_throws', { project: P({ assets: [{ id: { toString: 1 }, name: 'qa-x.png', mime: 'image/png' }] }), vision: SEES });
  await addTurn('id_backslash', { project: P({ assets: [asset('img\\qa-1', 'qa-back.png')] }), vision: SEES });
  await addTurn('path_outside', { project: P({ assets: [asset('../qa-outside.png', 'qa-outside.png')] }), outside: PNG, vision: SEES });
  await addTurn('path_inside', { project: P({ assets: [asset('qa-sub/../img-qa-1', 'qa-inside.png')] }), ...IMG1, vision: SEES });
  await addTurn('path_sub_missing', { project: P({ assets: [asset('qa-sub/img-qa-1', 'qa-sub.png')] }), ...IMG1, vision: SEES });
  await addTurn('asset_null_throws', { project: P({ assets: [null] }) });
  await addTurn('asset_primitives', { project: P({ assets: ['qa', 7, true, [], asset('img-qa-1', 'qa-ok.png')] }), ...IMG1, vision: SEES });

  // ── Names ───────────────────────────────────────────────────────────────────────────────
  const NAMES = [['astral', 'QA 😀 chart.png'], ['long', `qa-${'n'.repeat(495)}😀-long.png`], ['lone', 'qa-\ud83d-lone.png'], ['raw', 'qa line‮mark.png'],
    ['quotes', 'qa "quoted" \\ back.png'], ['number', 7], ['null', null], ['bool', false], ['list', ['qa', 'b']], ['nel', 'qa\u0085next⁦.png']];
  for (const [label, n] of NAMES) {
    await addTurn(`name_${label}_seen`, { project: P({ assets: [{ id: 'img-qa-1', name: n, mime: 'image/png' }] }), ...IMG1, vision: SEES });
    await addTurn(`name_${label}_described`, { project: P({ assets: [{ id: 'img-qa-1', name: n, mime: 'image/png' }, asset('img-qa-2', 'qa-gone.png')] }), ...IMG1, ...VR });
  }
  await addTurn('name_missing_key', { project: P({ assets: [{ id: 'img-qa-1', mime: 'image/png' }, { id: 'img-qa-2', mime: 'image/png' }] }), ...IMG1, ...VR });
  await addTurn('name_missing_key_blind', { project: P({ assets: [{ id: 'img-qa-1', mime: 'image/png' }] }), ...IMG1 });
  await addTurn('name_throws_missing', { project: P({ assets: [{ id: 'img-qa-1', name: { toString: 1 }, mime: 'image/png' }] }), vision: SEES });
  await addTurn('name_throws_loaded', { project: P({ assets: [{ id: 'img-qa-1', name: { toString: 1 }, mime: 'image/png' }] }), ...IMG1, vision: SEES });

  // ── The turn's own image ids (#1512) ────────────────────────────────────────────────────
  const TWO_ASSETS = { project: P({ assets: [asset('img-qa-1', 'qa-one.png'), asset('img-qa-2', 'qa-two.png')] }), images: { 'img-qa-1': PNG, 'img-qa-2': JPEG } };
  await addTurn('turn_ids_subset', { ...TWO_ASSETS, vision: SEES, body: { turnImageIds: ['img-qa-2'] } });
  await addTurn('turn_ids_none', { ...TWO_ASSETS, vision: SEES, body: { turnImageIds: [] } });
  await addTurn('turn_ids_unknown', { ...TWO_ASSETS, vision: SEES, body: { turnImageIds: ['img-qa-9', null, 5, { id: 'img-qa-1' }] } });
  await addTurn('turn_ids_text', { ...TWO_ASSETS, vision: SEES, body: { turnImageIds: 'img-qa-1' } });
  await addTurn('turn_ids_number', { project: P({ assets: [{ id: 5, name: 'qa-five.png', mime: 'image/png' }, asset('img-qa-1', 'qa-one.png')] }), ...IMG1, vision: SEES,
    body: { turnImageIds: [5] } });
  await addTurn('turn_ids_null_asset_throws', { project: P({ assets: [asset('img-qa-1', 'qa-one.png'), null] }), ...IMG1, body: { turnImageIds: ['img-qa-1'] } });
  await addTurn('turn_ids_primitive_assets', { project: P({ assets: ['qa', asset('img-qa-1', 'qa-one.png')] }), ...IMG1, vision: SEES, body: { turnImageIds: ['img-qa-1'] } });
  await addTurn('turn_ids_described', { ...TWO_ASSETS, ...VR, body: { turnImageIds: ['img-qa-1'] } });
  await addTurn('turn_ids_missing_unattached', { project: P({ assets: [asset('img-qa-1', 'qa-one.png'), asset('img-qa-2', 'qa-gone.png'), asset('img-qa-3', 'qa-three.png')] }),
    ...IMG1, vision: SEES, body: { turnImageIds: ['img-qa-1', 'img-qa-2'] } });

  // ── A model that cannot see ─────────────────────────────────────────────────────────────
  await addTurn('blind', { ...ONE });
  await addTurn('blind_two', { ...TWO_ASSETS });
  for (const [label, vision] of [['projector', { supported: false, reason: 'The inference server needs a multimodal projector (mmproj) configured.' }],
    ['projector_upper', { supported: false, reason: 'QA PROJECTOR missing' }], ['mmproj_only', { supported: false, reason: 'qa MmProj' }],
    ['reason_undefined', { supported: false }], ['reason_null', { supported: false, reason: null }], ['reason_number', { supported: 0, reason: 5 }],
    ['reason_list', { supported: false, reason: ['mmproj', 'x'] }], ['reason_long', { supported: false, reason: `QA projector ${'r'.repeat(185)}😀${'s'.repeat(100)}` }],
    ['reason_astral_cut', { supported: false, reason: `mmproj${'x'.repeat(193)}😀tail` }], ['reason_object', { supported: false, reason: { qa: 1 } }],
    ['supported_null', { supported: null, reason: 'QA' }]]) {
    await addTurn(`blind_${label}`, { ...ONE, vision });
  }
  await addTurn('blind_projector_no_hook', { ...ONE, vision: { supported: false, reason: 'projector' }, evidence: false });
  await addTurn('blind_reason_throws', { ...ONE, vision: { supported: false, reason: { toString: 1 } } });

  // ── A Vision role describes ─────────────────────────────────────────────────────────────
  await addTurn('described', { ...ONE, ...VR });
  await addTurn('described_sees_too', { ...ONE, ...VR, vision: SEES });
  await addTurn('described_three', { project: P({ assets: [asset('img-qa-1', 'qa-a.png'), asset('img-qa-2', 'qa-b.jpg', 'image/jpeg'), asset('img-qa-3', 'qa-c.gif', 'image/gif')] }),
    images: { 'img-qa-1': PNG, 'img-qa-2': JPEG, 'img-qa-3': GIF }, ...VR });
  await addTurn('described_one_missing', { project: P({ assets: [asset('img-qa-1', 'qa-a.png'), asset('img-qa-2', 'qa-gone.png')] }), ...IMG1, ...VR });
  for (const [label, d] of [['trim', { text: '  \n QA trimmed description.\t ' }], ['astral', { text: 'QA 😀 description with </untrusted> and $& inside' }],
    ['lone', { text: 'QA \udc00 lone description' }], ['list', { json: { choices: [{ message: { content: ['QA a', 'QA b'] } }] } }],
    ['number', { json: { choices: [{ message: { content: 42 } }] } }], ['object', { json: { choices: [{ message: { content: {} } }] } }],
    ['choices_object', { json: { choices: { 0: { message: { content: 'QA from an object' } } } } }], ['choices_text', { json: { choices: 'QA' } }],
    ['finish_other', { text: 'QA stopped early', finish: 'content_filter' }], ['cap_exact', { big: 'cap' }], ['slow', { text: 'QA slow description', delayMs: 30 }]]) {
    await addTurn(`described_${label}`, { ...ONE, ...VR, describe: d, ...(label === 'slow' ? { stageMs: 5000 } : {}) });
  }
  await addTurn('described_vision_number', { ...ONE, roles: { vision: 5 } });
  await addTurn('described_no_lease_hook', { ...ONE, ...VR, lease: false });
  await addTurn('described_headers', { ...ONE, ...VR, headers: { default: { 'X-QA-Extra': 'qa', 'Content-Type': 'text/qa' } } });
  await addTurn('described_long_question', { ...ONE, ...VR, message: `QA ${'q'.repeat(495)}😀 and the rest of a long question` });
  await addTurn('described_question_lone', { ...ONE, ...VR, message: 'QA \ud83d lone question' });
  for (const [label, baseUrl] of [['slashes', `${PROVIDER}/v1///`], ['no_v1', `${PROVIDER}/`], ['v1_twice', `${PROVIDER}/v1/v1`], ['undefined', undefined], ['number', 5]]) {
    const row = { id: 'default', label: 'QA local', ...(baseUrl === undefined ? {} : { baseUrl }) };
    await addTurn(`describe_row_url_${label}`, { ...ONE, ...VR, describeRow: row });
  }

  // ── The description does not come ───────────────────────────────────────────────────────
  for (const [label, d] of [['empty', { text: '' }], ['blank', { text: ' \n\t ' }], ['length', { text: 'QA cut description', finish: 'length' }],
    ['no_choices', { json: {} }], ['not_json', { raw: 'QA not json' }], ['empty_body', { raw: '' }], ['null_body', { json: null }], ['status_500', { status: 500 }], ['status_404', { status: 404 }],
    ['over_cap', { big: 'over' }], ['throws', { throws: 'QA connection refused' }], ['content_throws', { json: { choices: [{ message: { content: { toString: 1 } } }] } }]]) {
    await addTurn(`describe_${label}`, { ...ONE, ...VR, describe: d, vision: SEES });
    if (label === 'status_500' || label === 'throws') await addTurn(`describe_${label}_blind`, { ...ONE, ...VR, describe: d });
  }
  await addTurn('describe_hang', { ...ONE, ...VR, describe: { hang: true }, stageMs: 50, vision: SEES });
  await addTurn('describe_hang_blind', { ...ONE, ...VR, describe: { hang: true }, stageMs: 1500 });
  await addTurn('describe_left', { ...ONE, ...VR, describe: { leave: true }, vision: SEES, refusedOk: true });
  for (const [label, answer] of [['code', 'code'], ['status', 'status'], ['bare', 'bare']]) await addTurn(`lease_${label}`, { ...ONE, ...VR, leaseAnswer: answer, vision: SEES });
  await addTurn('lease_hang', { ...ONE, ...VR, leaseAnswer: 'hang', stageMs: 50, vision: SEES });
  await addTurn('lease_left', { ...ONE, ...VR, leaseAnswer: 'leave', vision: SEES, refusedOk: true });

  // ── Where the description is asked ──────────────────────────────────────────────────────
  await addTurn('describe_row_null', { ...ONE, ...VR, describeRow: null, vision: SEES });
  await addTurn('describe_row_other_id', { ...ONE, ...VR, describeRow: { id: 'qa-second', baseUrl: `${PROVIDER}/v1` }, vision: SEES });
  await addTurn('describe_row_external', { ...ONE, ...VR, describeRow: { id: 'default', label: 'QA local', baseUrl: `${PROVIDER}/v1`, external: true } });
  await addTurn('describe_row_text', { ...ONE, ...VR, describeRow: 'default', vision: SEES });
  await addTurn('roles_null', { ...ONE, roles: null, vision: SEES });
  await addTurn('roles_without_vision', { ...ONE, roles: { fast: 'qa-model' }, vision: SEES });
  await addTurn('roles_vision_empty', { ...ONE, roles: { vision: '' }, vision: SEES });
  await addTurn('roles_text', { ...ONE, roles: 'qa-vision', vision: SEES });
  await addTurn('second_provider_seen', { ...ONE, project: P({ provider: 'qa-second', assets: ONE.project.assets }), ...VR, vision: SEES, refusedOk: true });
  await addTurn('second_provider_blind', { ...ONE, project: P({ provider: 'qa-second', assets: ONE.project.assets }), ...VR, vision: { supported: false, reason: 'mmproj' }, refusedOk: true });

  // ── An external provider (rule 3) ───────────────────────────────────────────────────────
  await addTurn('external', { ...ONE, project: P({ provider: 'qa-cloud', model: 'qa-cloud-model', assets: ONE.project.assets }), ...VR, vision: SEES, refusedOk: true });
  await addTurn('external_unlabelled', { ...ONE, project: P({ provider: 'qa-cloud-unlabelled', model: 'qa-cloud-model', assets: ONE.project.assets }), vision: SEES, refusedOk: true });
  await addTurn('external_missing_unattached', { project: P({ provider: 'qa-cloud', model: 'qa-cloud-model',
    assets: [asset('img-qa-1', 'qa-one.png'), asset('img-qa-2', 'qa-gone.png'), asset('img-qa-3', 'qa-three.png')] }), ...IMG1, vision: SEES, refusedOk: true,
    body: { turnImageIds: ['img-qa-1', 'img-qa-2'] } });
  await addTurn('external_only_missing', { project: P({ provider: 'qa-cloud', model: 'qa-cloud-model', assets: [asset('img-qa-2', 'qa-gone.png')] }), refusedOk: true });

  // ── Several notes on one wire ───────────────────────────────────────────────────────────
  const AUTO = { routing: 'auto' };
  const CODE_ROLES = { fast: 'qa-fast', smart: 'qa-smart', code: 'qa-code' };
  await addTurn('code_route_seen', { project: P({ ...AUTO, assets: [asset('img-qa-1', 'qa-a.png'), asset('img-qa-2', 'qa-gone.png')] }), ...IMG1, roles: CODE_ROLES,
    classification: 'code', vision: SEES });
  await addTurn('code_route_described', { project: P({ ...AUTO, assets: [asset('img-qa-1', 'qa-a.png'), asset('img-qa-2', 'qa-gone.png'), asset('img-qa-3', 'qa-c.png')] }),
    ...IMG1, roles: { ...CODE_ROLES, vision: 'qa-vision' }, classification: 'code', body: { turnImageIds: ['img-qa-1', 'img-qa-2'] } });
  await addTurn('code_route_blind', { project: P({ ...AUTO, assets: [asset('img-qa-1', 'qa-a.png')] }), ...IMG1, roles: CODE_ROLES, classification: 'code' });
  await addTurn('code_route_no_images', { project: P({ ...AUTO }), roles: CODE_ROLES, classification: 'code' });
  await addTurn('fast_route_seen', { project: P({ ...AUTO, assets: [asset('img-qa-1', 'qa-a.png')] }), ...IMG1, roles: { ...CODE_ROLES, vision: 'qa-vision' },
    classification: 'fast', vision: SEES });
  const HISTORY = [{ role: 'user', content: 'QA q1' }, { role: 'assistant', content: 'QA a1' }];
  await addTurn('compact_images', { project: P({ assets: [asset('img-qa-1', 'qa-a.png'), asset('img-qa-2', 'qa-gone.png'), asset('img-qa-3', 'qa-c.png')] }), ...IMG1, ...VR,
    vision: SEES, message: '', body: { compactOnly: true, chatId: 'qa-compact-chat', history: HISTORY, turnImageIds: ['img-qa-1', 'img-qa-2'] } });
  await addTurn('compact_code_route', { project: P({ ...AUTO, assets: [asset('img-qa-1', 'qa-a.png')] }), ...IMG1, roles: CODE_ROLES, classification: 'code',
    message: '', body: { compactOnly: true, chatId: 'qa-compact-chat', history: HISTORY } });

  // ── Diary ───────────────────────────────────────────────────────────────────────────────
  await addTurn('diary_extras_no_images', { project: { id: diaryExtras.PROJECT_ID, name: 'QA extras' }, refusedOk: true });
  await addTurn('diary_extras_images', { project: { id: diaryExtras.PROJECT_ID, name: 'QA extras', assets: [asset('img-qa-1', 'qa-diary.png')] }, ...IMG1, vision: SEES, refusedOk: true });

  // ── The description cache ───────────────────────────────────────────────────────────────
  const C = { ...ONE, ...VR, group: 'qa-cache-1' };
  await addTurn('cache_first', { ...C });
  await addTurn('cache_hit', { ...C, describe: { text: 'QA never asked' } });
  await addTurn('cache_hit_sees', { ...C, vision: SEES, describe: { text: 'QA never asked' } });
  await addTurn('cache_other_question', { ...C, message: 'QA what is the total?', describe: { text: 'QA description for the total.' } });
  await addTurn('cache_other_user', { ...C, user: 'qa-other-user', describe: { text: 'QA description for the other user.' } });
  await addTurn('cache_other_project', { ...C, project: P({ id: 'qa-project-2', assets: ONE.project.assets }), describe: { text: 'QA description in project two.' } });
  await addTurn('cache_other_headers', { ...C, headers: { default: { 'X-QA-Extra': 'qa' } }, describe: { text: 'QA description with headers.' } });
  await addTurn('cache_other_bytes', { ...C, images: { 'img-qa-1': JPEG }, describe: { text: 'QA description of new bytes.' } });
  await addTurn('cache_other_name_same_key', { ...C, project: P({ assets: [asset('img-qa-1', 'qa-renamed.png')] }), describe: { text: 'QA never asked' } });
  await addTurn('cache_other_model', { ...C, roles: { vision: 'qa-vision-2' }, describe: { text: 'QA description by model two.' } });
  await addTurn('cache_failed_not_kept', { ...C, message: 'QA failing question', describe: { status: 500 }, vision: SEES });
  await addTurn('cache_failed_again', { ...C, message: 'QA failing question', describe: { text: 'QA description after a failure.' } });
  await addTurn('cache_expired', { ...C, expire: true, describe: { text: 'QA description after expiry.' } });
  await addTurn('cache_after_expiry_hit', { ...C, describe: { text: 'QA never asked' } });
  await addTurn('cache_off_box_not_asked', { ...C, project: P({ provider: 'qa-second', assets: ONE.project.assets }), vision: SEES, refusedOk: true });
  // Bounded at 64: the 65th description pushes the oldest out.
  const E = { ...ONE, ...VR, group: 'qa-cache-64' };
  for (let k = 0; k < 66; k += 1) await addTurn(`cache_fill_${k}`, { ...E, message: `QA question ${k}`, describe: { text: `QA description ${k}.` } });
  await addTurn('cache_evicted_asked_again', { ...E, message: 'QA question 0', describe: { text: 'QA description 0 again.' } });
  await addTurn('cache_kept_hit', { ...E, message: 'QA question 65', describe: { text: 'QA never asked' } });
  await addTurn('cache_refresh_existing', { ...E, message: 'QA question 3', expire: true, describe: { text: 'QA description 3 refreshed.' } });

  // ── Random turns ────────────────────────────────────────────────────────────────────────
  const POOL_NAMES = ['qa-a.png', 'QA 😀.jpg', 'qa "b".gif', 'qa c.webp', `qa-${'l'.repeat(300)}.png`, 'qa-\udc00.png', 7, null];
  const POOL_MIMES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp', undefined, ''];
  const POOL_BYTES = [PNG, JPEG, GIF, WEBP, EMPTY, TWO];
  const POOL_VISION = [SEES, BLIND, { supported: false, reason: 'mmproj missing' }, { supported: 1 }, { supported: false }];
  const POOL_DESCRIBE = [{ text: 'QA random description.' }, { text: '' }, { status: 500 }, { throws: 'QA down' }, { text: 'QA cut', finish: 'length' }, { raw: '' },
    { json: { choices: [] } }, { text: '  QA padded  ' }];
  const POOL_PROVIDER = [undefined, undefined, undefined, 'qa-second', 'qa-cloud'];
  for (let n = 0; n < 140; n += 1) {
    const count = int(0, 4);
    const assets = Array.from({ length: count }, (_, i) => {
      const a = { id: `img-qa-${i + 1}`, name: pick(POOL_NAMES) };
      const mime = pick(POOL_MIMES);
      if (mime !== undefined) a.mime = mime;
      return a;
    });
    const images = {};
    for (const a of assets) if (rand() < 0.8) images[a.id] = pick(POOL_BYTES);
    const provider = pick(POOL_PROVIDER);
    const project = P({ assets, ...(provider ? { provider, ...(provider === 'qa-cloud' ? { model: 'qa-cloud-model' } : {}) } : {}) });
    const ids = rand() < 0.25 ? assets.filter(() => rand() < 0.5).map((a) => a.id).concat(rand() < 0.2 ? ['img-qa-9'] : []) : undefined;
    const spec = { project, images, vision: pick(POOL_VISION), ...(rand() < 0.5 ? VR : rand() < 0.5 ? { roles: null } : {}), describe: pick(POOL_DESCRIBE),
      ...(rand() < 0.15 ? { evidence: false } : {}), ...(rand() < 0.1 ? { lease: false } : {}), ...(rand() < 0.1 ? { leaseAnswer: 'code' } : {}),
      message: rand() < 0.2 ? `QA ${'m'.repeat(int(480, 520))}😀` : 'QA random question', refusedOk: true,
      body: { ...(ids ? { turnImageIds: ids } : {}), ...(rand() < 0.08 ? { compactOnly: true, chatId: 'qa-random-chat', history: HISTORY } : {}) } };
    if (spec.body.compactOnly) spec.message = '';
    await addTurn(`random_${n}`, spec);
  }
  delete process.env.NOEVIA_CONTRACT_FIXED_NOW;

  // ── Differences ─────────────────────────────────────────────────────────────────────────
  // The port reads an asset only by a plain file name; Node reads path.join(assetDir, id) for any text id.
  const plain = (id) => typeof id === 'string' && id !== '' && id !== '.' && id !== '..' && !/[/\\\u0000]/.test(id);
  const differences = turns.filter((t) => {
    const assets = t.spec.project && Array.isArray(t.spec.project.assets) ? t.spec.project.assets : [];
    return assets.some((a) => a && typeof a === 'object' && typeof a.id === 'string' && !plain(a.id));
  }).map((t) => ({ kind: 'asset_path', name: t.name,
    // What Node read through such an id (a read by a plain id is the port's too).
    nodeRead: t.reads.filter(([p, ok]) => ok && !t.spec.project.assets.some((a) => a && plain(a.id) && p === `assets/${a.id}`)).map(([p]) => p),
    about: 'An asset id that is not a plain file name is a missing image here, without a read; Node reads path.join(assetDir, id).' }))
    // The describe-failed line of a body that is not JSON carries V8's JSON.parse message in Node.
    .concat(turns.filter((t) => t.calls.some((c) => c[0] === 'fetch') && typeof t.spec.describe?.raw === 'string' && t.spec.describe.raw !== '')
      .map((t) => ({ kind: 'parse_message', name: t.name, nodeLines: t.lines,
        about: "The [vision] describe-failed line of a reply that is not JSON says the port's NOT_JSON where Node writes V8's JSON.parse message." })));

  // ── Output ──────────────────────────────────────────────────────────────────────────────
  const facts = { providers: PROVIDERS, external: Object.fromEntries(Object.entries(PROVIDERS).map(([k, row]) => [k, egress.isExternalProvider(row)])),
    blind: BLIND, diaryExtrasId: diaryExtras.PROJECT_ID, cap: CAP, large: LARGE };
  const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
  const text = `{"version":1,"generator":"crates/chat-prepare/tests/record_vision.cjs",\n"node":${JSON.stringify(process.version)},\n`
    + `"sources":${JSON.stringify(sourcesRead())},\n"blocks":${JSON.stringify(blocks)},\n"facts":${JSON.stringify(facts)},\n`
    + `"turns":[\n${lines(turns)}\n],\n"differences":[\n${lines(differences)}\n]}\n`;
  const check = process.argv.indexOf('--check');
  const count = (f) => turns.filter(f).length;
  const counts = `${turns.length} turns (${count((t) => t.throws)} throw, ${count((t) => t.frame)} frames, ${count((t) => t.refused)} refused, `
    + `${count((t) => t.calls.some((c) => c[0] === 'fetch'))} described, ${count((t) => t.calls.some((c) => c[0] === 'visionProbe'))} probed), ${differences.length} differences`;
  if (check > 0) {
    const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
    if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
    process.stderr.write(`ok: ${counts} recorded as committed\n`, () => process.exit(0));
  } else {
    process.stderr.write(`recorded: ${counts}\n`);
    // The partial-save timers of chat-id turns would keep the process alive: leave once stdout is written.
    process.stdout.write(text, () => process.exit(0));
  }
})().catch((e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
