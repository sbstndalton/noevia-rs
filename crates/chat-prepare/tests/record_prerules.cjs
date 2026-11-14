#!/usr/bin/env node
'use strict';
// Recorded routing-mode pre-rules, the cloud hold of tool results and the router's input for crates/chat-prepare
// (slice P5d, part 3): the contract src/pre_rules.rs and src/router_input.rs are replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-prepare/tests/record_prerules.cjs > crates/chat-prepare/tests/fixtures/node-prerules.v1.json
//   ... node crates/chat-prepare/tests/record_prerules.cjs --check crates/chat-prepare/tests/fixtures/node-prerules.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Nothing from node_modules and no dav-parse.wasm. Every expectation comes from noevia-core's own code; the inputs are
// synthetic and QA-named (hand-made edges plus a seeded PRNG), passed through JSON first so Node reads exactly what is
// recorded:
//   preRule        routing-modes.cjs preRule(text) and preRule(text, { health: false }) (`flag`, `public`); a long
//                  text is recorded as `build`, a list of [piece, times] joined in order (the text is never stored)
//   hold           chat.cjs's own `cloudHold`/`holdForCloud` lines (cut out of the pinned chat.cjs and run as they are,
//                  over a stub routingModes.log and console), one turn per case: its route and toolboxes, then each
//                  tool result in order with whether it was held, and every log entry and console line
//   routerInput    routerInput(input, options) (options left out: the default; a field left out: undefined)
//   chunkCharsFor  chunkCharsFor({ limits: { maxStateChars } }) (left out: no limits)
// Each source file's sha256 is recorded, so a changed Node file needs a fresh recording. Node's console output goes to
// stderr; stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => process.stderr.write(`${a.map(String).join(' ')}\n`);
// Read besides the modules: chat.cjs, whose lines are run below without loading it.
const READ_FILES = ['server/chat.cjs'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex');
function sourcesRead() {
  const server = `${path.join(root, 'server')}${path.sep}`;
  const loaded = Object.keys(require.cache).filter((k) => k.startsWith(server)).map((k) => path.relative(root, k).split(path.sep).join('/'));
  if (loaded.some((k) => k.includes('node_modules/'))) throw new Error('a module from node_modules was loaded: the header says nothing is');
  return Object.fromEntries([...new Set([...READ_FILES, ...loaded])].sort().map((f) => [f, sha(f)]));
}
const serverRequire = (f) => require(path.join(root, 'server', f));
const rm = serverRequire('routing-modes.cjs');
const toolModes = serverRequire('tools-mode.cjs');

// ── chat.cjs's block ────────────────────────────────────────────────────────────────────────
const chatText = fs.readFileSync(path.join(root, 'server/chat.cjs'), 'utf8');
const START = '    let cloudHold = null;\n    const holdForCloud = (name, text) => {\n';
const END = '      return true;\n    };\n';
const once = (needle) => {
  const at = chatText.indexOf(needle);
  if (at < 0 || chatText.indexOf(needle, at + 1) >= 0) throw new Error(`chat.cjs holds ${JSON.stringify(needle.slice(0, 60))} ${at < 0 ? 'nowhere' : 'twice'}`);
  return at;
};
const blockStart = once(START);
const blockEnd = chatText.indexOf(END, blockStart) + END.length;
const BLOCK = chatText.slice(blockStart, blockEnd);
const blockLines = [chatText.slice(0, blockStart).split('\n').length, chatText.slice(0, blockEnd).split('\n').length - 1];
// Where the turn asks it (each tool result, before another round may carry it out), checked to be there.
for (const use of ['      if (holdForCloud(call.name, result)) return null;\n', '          holdForCloud(tc.name, result);\n']) once(use);
const makeHold = new Function('routeTarget', 'routingModes', 'isPublicTool', 'require', 'console',
  `${BLOCK}\nreturn { holdForCloud, held: () => cloudHold };`);

// ── Deterministic inputs ────────────────────────────────────────────────────────────────────
let seed = 0x2026_1114;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const pick = (list) => list[Math.floor(rand() * list.length)];
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));

// ── preRule ─────────────────────────────────────────────────────────────────────────────────
const preRule = [];
const rule = (text) => {
  const t = viaJson(text);
  preRule.push({ text: t, flag: rm.preRule(t), public: rm.preRule(t, { health: false }) });
};
const built = (build) => {
  const t = build.map(([piece, n]) => piece.repeat(n)).join('');
  preRule.push({ build, flag: rm.preRule(t), public: rm.preRule(t, { health: false }) });
};
const EDGES = [
  '', ' ', 'QA hello, nothing private here.',
  // SECRET_PATTERNS
  '-----BEGIN PRIVATE KEY-----', '-----BEGIN RSA PRIVATE KEY-----', '-----BEGIN rsa PRIVATE KEY-----', '-----BEGIN PUBLIC KEY-----',
  `-----BEGIN ${'A'.repeat(40)}PRIVATE KEY-----`, `-----BEGIN ${'A'.repeat(41)}PRIVATE KEY-----`,
  'Bearer qa0123456789abcdef', 'Bearer qa0123456789abcde', 'bearer qa0123456789abcdef', 'xBearer qa0123456789abcdef', 'éBearer qa0123456789abcdef',
  `Bearer${' '.repeat(8)}qa0123456789abcdef`, `Bearer${' '.repeat(9)}qa0123456789abcdef`, 'Bearer qa0123456789abcdef', 'Bearer\u0085qa0123456789abcdef',
  'Bearer﻿qa0123456789abcdef', 'Bearer qa0123456789abcdef', 'Bearer qa012345678😀abcdef',
  // SECRET_ASSIGN_RE and its value rules
  'password=hunter22', 'password = hunter2', 'password: letmein', 'password==hunter22', 'password::hunter22', 'password = =hunter22',
  'Password: hunter', 'PASSWORT = geheim1', 'passwd=abcdef', 'passwd=abcde', 'password=config.password', 'password=getPass()', 'password=your_pass',
  'password=xxxxxx', 'password=******', 'password=......', 'password=…', 'password=<redacted>', 'password=${QA_PASS}', 'password=changeme', 'password=example1',
  'password=null', 'password=None', 'password=undefined', 'password="hunter22"', "password='hunt'", 'password=`qa-secret`', 'password="xxxxxx"',
  'pin: 1234', 'pin=123', 'PIN = 123456789012', 'pin=1234567890123', 'passcode: 0000', 'pin: abcd', 'pinball=12345678',
  'token=abc12345', 'token=abcdefgh', 'token=value2', 'token=value10', 'token=value123', 'token=a.b1234567', 'token=fn(12345678)',
  'secret: Qa7x9Kp2', 'secret: Qa7x9Kp', 'api key = Qa7x9Kp2Lm', 'api_key=Qa7x9Kp2Lm', 'API-KEY: Qa7x9Kp2Lm', 'apikey=Qa7x9Kp2Lm', 'api  key=Qa7x9Kp2Lm',
  'access token: Qa7x9Kp2Lm', 'access_token=Qa7x9Kp2Lm', 'auth-token = "abcdefgh"', 'mytoken=abc12345', '_token=abc12345', 'étoken=abc12345',
  'token==x password=hunter22', 'token::x token=abc12345', 'token=:abc12345', 'token:=abc12345', 'token =  abc12345', 'token　=　abc12345',
  'token\u0085=abc12345', 'token=abc\u0085d12345', 'token=😀😀1a', 'token=😀😀😀1', 'password=😀😀😀', 'password=😀😀', 'token=\ud800\ud8001234',
  `token=${'a1'.repeat(100)}`, `token=${'a1'.repeat(101)}`, `token=${' '.repeat(8)}abc12345`, `token=${' '.repeat(9)}abc12345`, `token${' '.repeat(9)}=abc12345`,
  'ſecret=Qa7x9Kp2', 'toKen=abc12345', 'toKen=abc12345', 'password=hunter22;', 'password=hunt;er22', 'token=(abc12345', 'token=abc)12345',
  // JWT and key shapes
  'eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJxYSJ9.c2lnbmF0dXJl', 'eyJhbGciOiJ9.eyJzdWIiOiJxYSJ9.c2lnbmF0dXJl', 'eyJhbGciOiJIUzI1NiJ9.eyJzdWIi.c2lnbmF0',
  'eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJxYSJ9', 'xeyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJxYSJ9.c2lnbmF0dXJl', 'eyJhbGciOiJIUzI1NiJ9..eyJzdWIiOiJxYSJ9.c2lnbmF0dXJl',
  'sk-Qa7x9Kp2Lm4Nz8Rt5Vw1Yb3D', 'sk-Qa7x9Kp2Lm4Nz8Rt5Vw1Yb3', 'ghp_QaQaQaQaQaQaQaQaQaQaQaQa', 'ghp_123456789012345678901234', 'qa_Q12345678901234567890123',
  'Sk-Qa7x9Kp2Lm4Nz8Rt5Vw1Yb3D', 'qaqaqaqaq-Qa7x9Kp2Lm4Nz8Rt5Vw1Yb3D', 'q-Qa7x9Kp2Lm4Nz8Rt5Vw1Yb3D', 'sk-Qa7x9Kp2Lm4Nz8Rt5Vw1Yb3D-', 'sk-_123456789012345678901234',
  'cowork-chat-context-c-1794571200000-qa1x9z/notes.md', 'cowork-chat-context-c-1794571200000-qa1x9zzzz/notes.md', 'xcowork-chat-context-c-1794571200000-qa1x9z/n',
  'cowork-chat-context-c-1794571200000-qa1x9z', 'files/cowork-chat-context-c-123456789-qa/x', 'see data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==',
  'DATA:IMAGE/JPEG;BASE64,sk-Qa7x9Kp2Lm4Nz8Rt5Vw1Yb3D', 'data:image/svg+xml;base64,sk-Qa7x9Kp2Lm4Nz8Rt5Vw1Yb3D', 'data:image/png;base64,sk-Qa7x9Kp2Lm4Nz8Rt5Vw1Yb3D==x',
  // IBAN and cards
  'DE89 3704 0044 0532 0130 00', 'DE89370400440532013000', 'DE89 3704 0044 0532 0130 01', 'GB82 WEST 1234 5698 7654 32', 'GB82WEST12345698765432', 'gb82 west 1234 5698 7654 32',
  'XDE89370400440532013000', 'DE8937040044053201300', 'NO9386011117947', 'NO938601111794', 'MT84MALT011000012345MTLCAST001S', 'DE89  3704 0044 0532 0130 00',
  '4111 1111 1111 1111', '4111-1111-1111-1111', '4111111111111111', '4111111111111112', '378282246310005', '4111 1111 1111', '4111111111111111111', '41111111111111111110',
  '4111--1111-1111-1111', 'x4111111111111111', '4111111111111111x', '1234 5678 9012 3456 DE89370400440532013000', '٤١١١١١١١١١١١١١١١',
  // health
  'I was diagnosed with diabetes.', 'The diagnosis was asthma', 'Diagnose: Hypertensive crisis', 'diagnoses of COVID-19', 'diagnosed', 'diabetes', 'my diagnosis',
  'my medical records', 'their  medical\thistory', 'her medical record', 'his    diagnosis', 'diagnosed heart attack', 'diagnosed heart  attack', 'diagnosed anaemia and anemia',
  `diagnosis${' '.repeat(51)}cancer`, `diagnosis${' '.repeat(52)}cancer`, `cancer${' '.repeat(54)}diagnosis`, `cancer${' '.repeat(55)}diagnosis`, `xcancer${' '.repeat(53)}diagnosis`,
  `diagnosis${'😀'.repeat(26)}cancer`, `diagnosis${'😀'.repeat(25)}cancer`, 'diagnosisx cancer', 'adiagnosis cancer', 'diagnosis cancers', 'diagnosis hiv', 'diagnosis HIV',
  'mydiagnosis', 'my diagnosis', 'my\u0085diagnosis', 'diagnosed with diabetes and more', 'ı diagnosed with dıabetes',
];
for (const t of EDGES) rule(t);
// The scan limit (400,000 code units): a rule cut by it, and the same rule just inside.
for (const pad of [399_980, 399_983, 399_984, 399_990, 399_996, 400_000]) built([['a ', pad / 2], ['password=hunter22', 1]]);
for (const pad of [199_990, 199_992, 199_995]) built([['😀', pad], ['password=hunter22', 1]]);
built([['diagnosed with diabetes ', 1], ['x', 399_976]]);
built([['x ', 199_985], ['diagnosed with diabetes', 1]]);
built([['cowork-chat-context-c-1794571200000-qa1x9z/', 9_300], ['sk-Qa7x9Kp2Lm4Nz8Rt5Vw1Yb3D', 1]]);
// Random texts from pieces that trip, nearly trip or sit next to every rule.
const PIECES = [
  'password', 'Password', 'PASSWORT', 'passwd', 'passcode', 'pin', 'PIN', 'api key', 'api_key', 'API-KEY', 'apikey', 'secret', 'access token', 'auth_token',
  'token', 'Bearer', '-----BEGIN RSA PRIVATE KEY-----', '=', '==', ':', '::', ' ', '  ', '\t', ' ', ' ', '　', '﻿', '\u0085', '"', "'", '`',
  ';', ',', ')', '(', '.', 'hunter22', 'letmein', '1234', '123456', 'abc', 'value2', 'token10', 'config.token', 'getToken()', 'null', 'TRUE', 'xxxx', '****',
  '...', '…', '<secret>', '${SECRET}', 'your_key', 'changeme', 'example123', 'Qa7x9Kp2Lm4Nz8Rt', 'eyJhbGciOiJIUzI1NiJ9', 'eyJzdWIiOiJxYSJ9',
  'c2lnbmF0dXJlX3Rlc3Q', 'sk-', 'ghp_', 'qa-', 'A1b2C3d4E5f6G7h8I9j0K1l2M3n4', 'DE89 3704 0044 0532 0130 00', 'DE89370400440532013000',
  'GB82 WEST 1234 5698 7654 32', 'DE89 3704 0044 0532 0130 01', '4111 1111 1111 1111', '4111-1111-1111-1111', '4111111111111112', '378282246310005',
  'diagnosed', 'diagnosis', 'Diagnose', 'with', 'diabetes', 'cancer', 'heart attack', 'anaemia', 'my medical records', 'their diagnosis',
  'cowork-chat-context-c-1794571200000-qa1x9z/', 'data:image/png;base64,iVBORw0KGgo=', 'DATA:IMAGE/JPEG;BASE64,AAAA', 'data:image/svg+xml;base64,AAAA',
  'ſecret', 'K', 'ı', '😀', '\ud800', '\udfff', 'é', 'x'.repeat(30), '0123456789', 'Ab', 'Z9', '_', '-', '$', '/', 'qa', '\n', '\r',
];
for (let i = 0; i < 2500; i++) {
  const n = int(1, 12);
  let t = '';
  for (let k = 0; k < n; k++) t += pick(PIECES);
  rule(t);
}

// ── hold ────────────────────────────────────────────────────────────────────────────────────
const tool = (name) => ({ type: 'function', function: { name, description: 'QA', parameters: { type: 'object', properties: {} } } });
const BOXES = [
  { id: 'web-search', tools: [tool('web_search'), tool('web_fetch')] },
  { id: 'offline-wikipedia', tools: [tool('wikipedia_search')] },
  { id: 'web-crawl', tools: [tool('crawl_page')] },
  { id: 'nextcloud', tools: [tool('nextcloud_read'), tool('web_search_files')] },
  { id: 'core', tools: [tool('read_project_file'), tool('list_files')] },
  { id: 'qa-mixed', tools: [tool('wikipedia_search'), tool('qa_lookup')] },
];
const hold = [];
const holdCase = (route, boxes, calls) => {
  const logs = [];
  const lines = [];
  const routingModes = { log: (entry) => { logs.push(viaJson(entry)); if (entry.flag === 'card') throw new Error('QA log down'); } };
  const fakeConsole = { log: (...a) => lines.push(a.map(String).join(' ')) };
  const list = viaJson(boxes);
  const routeTarget = route === undefined ? undefined : route === null ? null : { route };
  const isPublicTool = (name) => toolModes.isPublicToolName(name, list);
  const requireRelative = (f) => serverRequire(f.replace(/^\.\//, ''));
  const h = makeHold(routeTarget, routingModes, isPublicTool, requireRelative, fakeConsole);
  const results = calls.map(([name, text]) => {
    const [n, t] = viaJson([name, text]);
    return { name: n, text: t, held: h.holdForCloud(n, t), public: isPublicTool(n) };
  });
  hold.push({ route: route === undefined ? 'undefined' : route, boxes: list, calls: results, held: h.held(), logs, lines });
};
const HOLD_TEXTS = ['QA weather is fine', 'password=hunter22', 'diagnosed with diabetes', 'DE89370400440532013000', '4111111111111111', 'my medical records'];
for (const route of ['cloud', 'local', null, undefined]) holdCase(route, BOXES, [['web_search', HOLD_TEXTS[2]], ['nextcloud_read', HOLD_TEXTS[0]], ['nextcloud_read', HOLD_TEXTS[2]], ['web_search', HOLD_TEXTS[1]]]);
holdCase('cloud', BOXES, [['diary_read', 'QA nothing'], ['web_search', HOLD_TEXTS[1]]]);
holdCase('cloud', BOXES, [['Diary-Search', ''], ['DIARY_x', 'x']]);
holdCase('cloud', BOXES, [['diaryread', 'QA'], ['diary', 'QA'], ['xdiary_read', 'QA'], ['diar_y', 'QA'], ['dıary_read', 'QA']]);
holdCase('cloud', BOXES, [['web_search', HOLD_TEXTS[4]], ['web_search', HOLD_TEXTS[1]]]);
holdCase('cloud', BOXES, [['web_search', HOLD_TEXTS[2]], ['web_search', HOLD_TEXTS[5]], ['wikipedia_search', HOLD_TEXTS[2]], ['crawl_page', HOLD_TEXTS[2]]]);
holdCase('cloud', BOXES, [['qa_unknown', HOLD_TEXTS[2]]]);
holdCase('cloud', BOXES, [['read_project_file', HOLD_TEXTS[0]], ['web_search_files', HOLD_TEXTS[5]]]);
holdCase('cloud', [], [['web_search', HOLD_TEXTS[2]]]);
holdCase('cloud', BOXES, [['', HOLD_TEXTS[3]]]);
for (let i = 0; i < 60; i++) {
  const names = ['web_search', 'web_fetch', 'wikipedia_search', 'crawl_page', 'nextcloud_read', 'read_project_file', 'qa_lookup', 'qa_unknown', 'diary_read', 'Diary-x'];
  const calls = [];
  for (let k = int(1, 4); k > 0; k--) calls.push([pick(names), pick(rand() < 0.5 ? HOLD_TEXTS : EDGES)]);
  holdCase(pick(['cloud', 'cloud', 'local']), BOXES.filter(() => rand() < 0.8), calls);
}

// ── routerInput ─────────────────────────────────────────────────────────────────────────────
const routerInput = [];
const route = (input, options) => {
  const i = viaJson(input);
  const o = viaJson(options);
  let out;
  try { out = options === undefined ? rm.routerInput(i) : rm.routerInput(i, o); } catch (e) { out = { threw: String(e && e.name) }; }
  const c = { input: i };
  if (options !== undefined) c.options = o;
  c.out = out;
  routerInput.push(c);
};
const CHARS = ['a', 'b', ' ', '\n', 'é', '😀', ' ', '\t', 'Q', 'A', '.', '\ud800', '\udc00'];
const textOf = (n) => { let s = ''; for (let k = 0; k < n; k++) s += pick(CHARS); return s; };
const entry = () => pick([
  () => ({ role: 'user', content: textOf(pick([0, 3, 40, 200, 1100])) }),
  () => ({ role: 'assistant', content: textOf(int(0, 120)) }),
  () => ({ role: 'tool', name: 'qa_tool', content: textOf(int(0, 120)), target: pick(['', 'qa/notes.md', 5]), args: pick(['', '{"path":"qa"}', null]) }),
  () => ({ role: 'user', content: [{ type: 'text', text: textOf(int(0, 50)) }, null, { type: 'image_url' }, { text: 7 }, 'qa', { type: 'text', text: textOf(int(0, 50)) }] }),
  () => ({ role: pick([undefined, null, 5, true, [1, 2], { a: 1 }, '']), content: textOf(int(0, 20)) }),
  () => pick([null, 0, '', false, 5, 'qa', [1], true]),
])();
const OPTIONS = [undefined, {}, { chunkChars: 150 }, { chunkChars: 200 }, { chunkChars: 999 }, { chunkChars: 1000.7 }, { chunkChars: 5000 }, { chunkChars: 0 },
  { chunkChars: -3 }, { chunkChars: null }, { chunkChars: 1e9 }, { maxChunks: 2 }, { maxChunks: 4 }, { maxChunks: 8 }, { maxChunks: 9 }, { maxChunks: 0 },
  { maxChunks: 0.5 }, { maxChunks: -1 }, { maxChunks: 2.9 }, { chunkChars: 300, maxChunks: 3 }, { chunkChars: 4000, maxChunks: 8 }];
route({ message: '', system: '', history: [], attachments: [] });
route({});
route({ message: '   ', system: '\n', history: 'qa', attachments: 'qa' });
route({ message: 'QA hello', system: 'QA system', history: [{ role: 'user', content: 'QA earlier' }], attachments: ['qa.pdf', '', 5, null, 'qa2.png'] });
route({ message: 'x'.repeat(1000) });
route({ message: 'x'.repeat(990) });
route({ message: 'x'.repeat(991) });
route({ message: '😀'.repeat(600), system: 'QA' });
route({ message: 'QA', history: Array.from({ length: 41 }, (_, k) => ({ role: 'user', content: `QA ${k}` })) });
route({ message: 'QA', history: Array.from({ length: 40 }, (_, k) => ({ role: 'user', content: `QA ${k}` })) });
route({ message: 'QA', history: [null, 0, ...Array.from({ length: 40 }, (_, k) => ({ role: 'user', content: `QA ${k}` }))] });
route({ message: 'QA', history: [{ role: { toString: 1 }, content: 'x' }] });
route({ message: 'QA', history: [{ role: 'tool', content: 'x'.repeat(5000), target: 'qa.md', args: '{}' }] }, { maxChunks: 4 });
for (const o of OPTIONS) route({ message: textOf(700), system: textOf(900), history: [entry(), entry(), entry()], attachments: ['qa.pdf'] }, o);
for (let i = 0; i < 160; i++) {
  const history = Array.from({ length: pick([0, 1, 3, 8, 40, 41]) }, entry);
  const input = { message: textOf(pick([0, 5, 100, 999, 1500])), system: textOf(pick([0, 10, 700, 1200])), history,
    attachments: Array.from({ length: int(0, 3) }, () => pick(['qa.pdf', '', 'QA notes 😀.md', 7])) };
  route(input, pick(OPTIONS));
}

// ── chunkCharsFor ───────────────────────────────────────────────────────────────────────────
const chunkCharsFor = [];
for (const limit of [undefined, null, 0, 199, 199.99, 200, 200.9, 517.5, 999.9, 1000, 1000.5, 4000, -5, 1e300]) {
  const backend = limit === undefined ? {} : { limits: { maxStateChars: limit } };
  chunkCharsFor.push({ ...(limit === undefined ? {} : { limit }), out: rm.chunkCharsFor(backend) });
}
chunkCharsFor.push({ none: true, out: rm.chunkCharsFor(undefined) });

// ── Output ──────────────────────────────────────────────────────────────────────────────────
const sources = sourcesRead();
const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
const text = `{"version":1,"generator":"crates/chat-prepare/tests/record_prerules.cjs",\n"node":${JSON.stringify(process.version)},\n`
  + `"sources":${JSON.stringify(sources)},\n"block":${JSON.stringify({ lines: blockLines, sha256: crypto.createHash('sha256').update(BLOCK).digest('hex') })},\n`
  + `"scanLimit":${JSON.stringify(400_000)},\n`
  + `"preRule":[\n${lines(preRule)}\n],\n"hold":[\n${lines(hold)}\n],\n"routerInput":[\n${lines(routerInput)}\n],\n`
  + `"chunkCharsFor":[\n${lines(chunkCharsFor)}\n]}\n`;
const check = process.argv.indexOf('--check');
const flags = {};
for (const c of preRule) flags[c.flag ?? 'none'] = (flags[c.flag ?? 'none'] || 0) + 1;
const incomplete = routerInput.filter((c) => c.out && c.out.complete === false).length;
const counts = `${preRule.length} preRule (${JSON.stringify(flags)}), ${hold.length} hold (${hold.filter((c) => c.held).length} held), `
  + `${routerInput.length} routerInput (${incomplete} incomplete), ${chunkCharsFor.length} chunkCharsFor`;
if (check > 0) {
  const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
  if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
  process.stderr.write(`ok: ${counts} recorded as committed\n`);
} else {
  process.stderr.write(`recorded: ${counts}\n`);
  process.stdout.write(text);
}
