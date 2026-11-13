'use strict';
// Test-only Node oracle for the front's chat edge (chat port S6). For every case it answers each step with core's own
// routes/chat.cjs, chat-mode.cjs and http.cjs (json, readBody) and, for the Diary local exchange, routes/diary.cjs, at
// the checkout given (CI: tests/fixtures/chat-edge-oracle.core-ref, the last core whose routes count the `llm:` budget),
// wired as server/index.cjs wired them: STORED_HISTORY_BYTES as the body cap, one `llm:<user>` budget of 60 a minute
// shared by both routes (auth.cjs createRateLimiter's fixed window; it needs better-sqlite3 to load, so the window is
// counted here the same way), the account's role and Diary add-on, and the code harness flag. A step that gets past the
// edge is recorded as where it went (`turn`: the chat loop, `cowork`: the code task start, `diary`: the Diary sidecar
// call) instead of an answer. No model runs; synthetic data only. A throw becomes http.cjs errorResponse, as index.cjs
// answers it.
//
//   node bins/noevia-server/tests/chat-edge-oracle.cjs <core-checkout> <out.json>
//
// Deterministic; CI regenerates it and compares with tests/fixtures/node-chat-edge.v1.json, which tests/chat_edge.rs
// replays through the front against a stand-in Core.
const fs = require('node:fs');
const path = require('node:path');
const { Readable } = require('node:stream');

const [core, out] = process.argv.slice(2);
if (!core || !out) throw new Error('Usage: node chat-edge-oracle.cjs <core-checkout> <out.json>');
const req = (m) => require(path.join(path.resolve(core), 'server', m));
const { createChatRoutes } = req('routes/chat.cjs');
const { createDiaryRoutes } = req('routes/diary.cjs');
const http = req('http.cjs');

const STORED_HISTORY_BYTES = 32 * 1024 * 1024;
const LLM_RATE_LIMIT = 60;
const PASSED = Symbol('passed');

// A body is { text } (UTF-8), { hex } (raw bytes) or { big: n } (a JSON object of exactly n bytes).
function bytesOf(spec) {
  if (spec.text !== undefined) return Buffer.from(spec.text, 'utf8');
  if (spec.hex !== undefined) return Buffer.from(spec.hex, 'hex');
  return Buffer.from(`{"message":"${'a'.repeat(spec.big - 14)}"}`, 'utf8');
}

/** auth.cjs createRateLimiter's count within one window (every step of a case runs inside one minute). */
function limiter() {
  const map = new Map();
  return (key, limit) => { const n = (map.get(key) || 0) + 1; map.set(key, n); return n > limit; };
}

function wired(account, passed) {
  const rate = limiter();
  const rateLimited = (userId) => rate(`llm:${userId}`, LLM_RATE_LIMIT);
  const chat = createChatRoutes({
    json: http.json, readBody: http.readBody, bodyCap: STORED_HISTORY_BYTES, rateLimited,
    diaryEnabled: () => account.diary,
    handleChat: async () => { passed.where = 'turn'; },
    harnessEnabled: () => account.harness,
    startCoworkTask: async () => { passed.where = 'cowork'; throw PASSED; },
  });
  const diary = createDiaryRoutes({
    json: http.json, readBody: http.readBody,
    readJson: async () => { passed.where = 'diary'; throw PASSED; },
    fetchJson: async () => { throw new Error('the oracle never reaches the sidecar'); },
    DIARY_BASE: 'http://diary.invalid', authService: { diaryEnabled: () => account.diary }, currentWorkspace: () => ({}),
    rateLimited, diary: { diaryHeaders: () => ({}), corpusSource: {}, log: () => {} },
  });
  return { chat, diary: diary.diary };
}

async function answer(routes, account, passed, route, spec) {
  passed.where = null;
  const r = Readable.from(spec ? [bytesOf(spec)] : []);
  const p = route === 'diary' ? '/api/diary/local-exchange' : '/api/chat';
  Object.assign(r, { method: 'POST', url: p, headers: {} });
  const res = { headers: {}, chunks: [] };
  res.writeHead = (status, h = {}) => { res.status = status; for (const [k, v] of Object.entries(h)) res.headers[k.toLowerCase()] = v; return res; };
  res.setHeader = (k, v) => { res.headers[k.toLowerCase()] = v; };
  res.end = (c) => { if (c !== undefined) res.chunks.push(Buffer.from(c)); };
  const authn = { user: { id: 'u-oracle', role: account.role } };
  try {
    await routes[route](r, res, { path: p, authn, url: new URL(`http://localhost${p}`) });
  } catch (err) {
    if (err !== PASSED) {
      const failure = http.errorResponse(err);
      http.json(res, failure.status, failure.body);
    }
  }
  if (passed.where) return { pass: passed.where };
  return {
    status: res.status,
    headers: { 'content-type': res.headers['content-type'], 'cache-control': res.headers['cache-control'] },
    body: Buffer.concat(res.chunks).toString('base64'),
  };
}

const MEMBER = { role: 'member', diary: false, harness: false };
const ADMIN = { role: 'admin', diary: true, harness: true };
const t = (text) => ({ text });
const chat = (body) => ['chat', body];
const one = (id, body, account = MEMBER) => ({ id, account, steps: [chat(body)] });
const P = JSON.stringify;
const ids = (n, len = 3) => Array.from({ length: n }, (_, i) => `${'b'.repeat(len - 1)}${i % 10}`);
const CASES = [
  // routes/chat.test.cjs: a well-formed POST reaches the loop.
  one('plain', t('{"message":"hi"}')),
  one('whitespace', t(' \n{"message":"hi"}\t ')),
  // Bad reads and bad JSON: 400 invalid JSON, then not an object.
  one('empty-body', t('')),
  one('invalid-json', t('{nope')),
  one('trailing-garbage', t('{"message":"a"} x')),
  one('bom', t('\ufeff{"message":"a"}')),
  one('null', t('null')),
  one('array', t('[]')),
  one('string', t('"hi"')),
  one('number', t('5')),
  one('number-overflow-top', t('1e400')),
  one('invalid-utf8', { hex: Buffer.concat([Buffer.from('{"message":"a'), Buffer.from([0xff, 0xc3, 0x28]), Buffer.from('b"}')]).toString('hex') }),
  one('invalid-utf8-structure', { hex: Buffer.concat([Buffer.from('{"message":"a"'), Buffer.from([0xff]), Buffer.from('}')]).toString('hex') }),
  one('deep-and-odd', t(`{"x":${'['.repeat(300)}1${']'.repeat(300)},"n":1e400,"s":"\\udc00\u{10FFFF}","message":"m"}`)),
  // The body cap: exactly STORED_HISTORY_BYTES passes, one byte more is the 413.
  one('exactly-cap', { big: STORED_HISTORY_BYTES }),
  one('over-cap', { big: STORED_HISTORY_BYTES + 1 }),
  // requestShapeError: mode.
  one('mode-chat', t('{"mode":"chat","message":"hi"}')),
  one('mode-escaped-chat', t('{"mode":"ch\\u0061t","message":"hi"}')),
  one('mode-code', t('{"mode":"code","message":"hi"}')),
  one('mode-null', t('{"mode":null,"message":"hi"}')),
  one('mode-number', t('{"mode":1,"message":"hi"}')),
  one('mode-case', t('{"mode":"Chat","message":"hi"}')),
  one('mode-duplicate-last-wins', t('{"mode":"code","mode":"chat","message":"hi"}')),
  one('mode-duplicate-last-refused', t('{"mode":"chat","mode":"code","message":"hi"}')),
  // A pinned Skill with Cowork, before the toolbox checks and before the Cowork role check.
  one('skill-cowork', t(P({ mode: 'cowork', projectId: 'p1', message: 'x', skill: `skill_${'0'.repeat(32)}@${'a'.repeat(64)}`, turnToolboxes: 'bad' }))),
  one('skill-null-cowork-member', t(P({ mode: 'cowork', projectId: 'p1', message: 'x', skill: null }))),
  one('skill-chat', t(P({ mode: 'chat', message: 'x', skill: 'anything' }))),
  one('skill-false-cowork', t(P({ mode: 'cowork', projectId: 'p1', message: 'x', skill: false }))),
  // turnToolboxes and turnImageIds.
  ...[['toolbox-string', 'web-search'], ['toolbox-number-item', [1]], ['toolbox-empty-id', ['']], ['toolbox-null', null],
    ['toolbox-object', { a: 'b' }], ['toolbox-21', ids(21)], ['toolbox-id-81', ['x'.repeat(81)]],
    ['toolbox-nested', [['a']]]].map(([id, v]) => one(id, t(P({ message: 'hi', turnToolboxes: v })))),
  ...[['toolbox-20', ids(20)], ['toolbox-empty-list', []], ['toolbox-id-80', ['x'.repeat(80)]],
    ['toolbox-emoji-40', ['😀'.repeat(40)]]].map(([id, v]) => one(id, t(P({ message: 'hi', turnToolboxes: v })))),
  one('toolbox-emoji-41', t(P({ message: 'hi', turnToolboxes: ['😀'.repeat(40) + 'x'] }))),
  one('toolbox-lone-surrogates-80', t(`{"message":"hi","turnToolboxes":["${'\\ud800'.repeat(80)}"]}`)),
  one('toolbox-lone-surrogates-81', t(`{"message":"hi","turnToolboxes":["${'\\ud800'.repeat(81)}"]}`)),
  one('images-13', t(P({ message: 'hi', turnImageIds: ids(13) }))),
  one('images-12', t(P({ message: 'hi', turnImageIds: ids(12) }))),
  one('images-string', t(P({ message: 'hi', turnImageIds: 'img' }))),
  one('toolbox-before-images', t(P({ message: 'hi', turnToolboxes: 'x', turnImageIds: 'y' }))),
  // Cowork: 403 for a member, 409 harness off, 409 outside a project, else the start.
  one('cowork-member', t(P({ mode: 'cowork', projectId: 'p1', message: 'x' }))),
  one('cowork-member-no-project', t(P({ mode: 'cowork', message: 'x' }))),
  one('cowork-harness-off', t(P({ mode: 'cowork', projectId: 'p1', message: 'x' })), { ...ADMIN, harness: false }),
  one('cowork-harness-off-no-project', t(P({ mode: 'cowork', message: 'x' })), { ...ADMIN, harness: false }),
  one('cowork-no-project', t(P({ mode: 'cowork', message: 'x' })), ADMIN),
  one('cowork-empty-project', t(P({ mode: 'cowork', projectId: '', message: 'x' })), ADMIN),
  one('cowork-number-project', t(P({ mode: 'cowork', projectId: 7, message: 'x' })), ADMIN),
  one('cowork-start', t(P({ mode: 'cowork', projectId: 'p1', repository: 'demo', message: 'fix the test' })), ADMIN),
  one('cowork-start-diary-space', t(P({ mode: 'cowork', projectId: 'p1', spaceId: 'diary', message: 'x' })), { ...ADMIN, diary: false }),
  // The Diary add-on gate, last.
  one('diary-off', t(P({ spaceId: 'diary', message: 'hi' }))),
  one('diary-on', t(P({ spaceId: 'diary', message: 'hi' })), { ...MEMBER, diary: true }),
  one('diary-escaped', t('{"spaceId":"di\\u0061ry","message":"hi"}')),
  one('diary-other-space', t(P({ spaceId: 'free', message: 'hi' }))),
  one('diary-case', t(P({ spaceId: 'Diary', message: 'hi' }))),
  one('diary-number', t(P({ spaceId: 1, message: 'hi' }))),
  one('diary-after-shape', t(P({ spaceId: 'diary', mode: 'x', message: 'hi' }))),
  one('admin-plain', t('{"message":"hi"}'), ADMIN),
  one('extra-fields', t('{"__proto__":{"mode":"code"},"constructor":1,"message":"m"}')),
];

// The budget, shared by the turn and the local exchange: counted before the body is read, also for a refused body,
// so the 61st call in the minute is refused whatever its body. Run on its own front (tests/chat_edge.rs).
const B = { ...MEMBER, diary: true };
const BUDGET = {
  id: 'budget-shared-with-the-diary-local-exchange', account: B, steps: [
    ...Array.from({ length: 30 }, () => chat(t('{"message":"hi"}'))),
    ...Array.from({ length: 20 }, () => ['diary', t('{"message":"hi"}')]),
    chat(t('{nope')), chat({ big: STORED_HISTORY_BYTES + 1 }), chat(t(P({ mode: 'x' }))),
    ...Array.from({ length: 7 }, () => chat(t('{"message":"hi"}'))),
    // 61 and on: refused before the body, each route in its own words.
    chat(t('{"message":"hi"}')), ['diary', t('{"message":"hi"}')], chat(t('{nope')), chat({ big: STORED_HISTORY_BYTES + 1 }),
    chat(t(P({ mode: 'cowork', projectId: 'p1', message: 'x' }))), ['diary', t('{"message":"hi"}')],
  ],
};

(async () => {
  const cases = [];
  for (const c of [...CASES, BUDGET]) {
    const passed = { where: null };
    const routes = wired(c.account, passed);
    const steps = [];
    for (const [route, spec] of c.steps) steps.push({ route, body: spec, node: await answer(routes, c.account, passed, route, spec) });
    cases.push({ id: c.id, account: c.account, steps });
  }
  fs.writeFileSync(out, `${JSON.stringify({ v: 1, limit: LLM_RATE_LIMIT, cases }, null, 1)}\n`);
})().catch((e) => { console.error(e); process.exit(1); });
