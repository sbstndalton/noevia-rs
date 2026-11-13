'use strict';
// Records tests/fixtures/code-harness.v1.json from noevia-core's own modules:
//
//   node crates/code-harness/tools/gen-fixtures.cjs <noevia-core>/server > crates/code-harness/tests/fixtures/code-harness.v1.json
//
// The checkout must have server/wasm/dav-parse.wasm at its pinned sha: code-tool-schemas.cjs
// validates through the stream-guard and code-actions crates inside that module, and their
// answers are recorded as they are. The sha256 of both source files goes into the fixture.
//
// Values are written in a tagged JSON form so that what JSON cannot hold survives the trip:
// {"$js":"undefined"|"NaN"|"Infinity"|"-Infinity"|"-0"|"function"} and, for long inputs,
// {"$js":"repeat","s":text,"n":count} (text.repeat(count)). Inputs are decoded back before
// Node sees them, so Node and the Rust tests read exactly the same values. A thrown Error is
// {"name","message"}; any other thrown value is {"value": tagged}.
//
// A case with `port` is a named difference (see the crate docs): `port` is Node's answer on the
// input the difference is defined by (`portArgs`), and the Rust port must give that instead.
const path = require('node:path');
const fs = require('node:fs');
const crypto = require('node:crypto');

const dir = process.argv[2];
if (!dir) { process.stderr.write('usage: gen-fixtures.cjs <noevia-core>/server\n'); process.exit(2); }
const metaFile = path.resolve(dir, 'code-meta.cjs');
const schemasFile = path.resolve(dir, 'code-tool-schemas.cjs');
const meta = require(metaFile);
const schemas = require(schemasFile);
const sha = (file) => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
// code-actions.cjs warns on stderr when its crate refuses a call; that is its own side effect.
console.warn = () => {};

// ── Tagged values ───────────────────────────────────────────────────────────────────────────
const TAG = '$js';
class Rep { constructor(s, n) { this.s = s; this.n = n; } }
const R = (s, n) => new Rep(s, n);
const own = (o, k, value) => Object.defineProperty(o, k, { value, enumerable: true, writable: true, configurable: true });

function enc(v) {
  if (v === undefined) return { [TAG]: 'undefined' };
  if (typeof v === 'number') {
    if (Number.isNaN(v)) return { [TAG]: 'NaN' };
    if (v === Infinity) return { [TAG]: 'Infinity' };
    if (v === -Infinity) return { [TAG]: '-Infinity' };
    if (Object.is(v, -0)) return { [TAG]: '-0' };
    return v;
  }
  if (typeof v === 'function') return { [TAG]: 'function' };
  if (v instanceof Rep) return { [TAG]: 'repeat', s: v.s, n: v.n };
  if (v === null || typeof v !== 'object') return v;
  if (Array.isArray(v)) return Array.from(v, enc);
  const out = {};
  for (const k of Object.keys(v)) {
    if (k === TAG) throw Error(`the key ${TAG} is reserved for tags`);
    own(out, k, enc(v[k]));
  }
  return out;
}

function dec(v) {
  if (Array.isArray(v)) return v.map(dec);
  if (v && typeof v === 'object') {
    if (Object.prototype.hasOwnProperty.call(v, TAG)) {
      switch (v[TAG]) {
        case 'undefined': return undefined;
        case 'NaN': return NaN;
        case 'Infinity': return Infinity;
        case '-Infinity': return -Infinity;
        case '-0': return -0;
        case 'repeat': return v.s.repeat(v.n);
        default: throw Error(`unknown tag ${v[TAG]}`);
      }
    }
    const out = {};
    for (const k of Object.keys(v)) own(out, k, dec(v[k]));
    return out;
  }
  return v;
}

/** What Node and the Rust test both read: the tagged form, through JSON text and back. */
const wire = (v) => JSON.parse(JSON.stringify(enc(v)));
const thrown = (e) => (e instanceof Error ? { name: e.name, message: e.message } : { value: enc(e) });
/** A returned Error (violationError, blockedError): its name, message and own properties, encoded by the caller. */
const errorRaw = (e) => ({ name: e.name, message: e.message, props: { ...e } });
function result(fn) {
  try { return { out: enc(fn()) }; } catch (e) { return { throws: thrown(e) }; }
}
const J = (text) => JSON.parse(text);
/** D2's defining input: the same value with every own `toString` key removed. */
function dropToString(v) {
  if (Array.isArray(v)) return v.map(dropToString);
  if (v && typeof v === 'object' && !(v instanceof Rep)) {
    const out = {};
    for (const k of Object.keys(v)) if (k !== 'toString') own(out, k, dropToString(v[k]));
    return out;
  }
  return v;
}
const hasToString = (v) => (Array.isArray(v) ? v.some(hasToString)
  : v && typeof v === 'object' && !(v instanceof Rep)
    ? Object.prototype.hasOwnProperty.call(v, 'toString') || Object.values(v).some(hasToString) : false);

// ── code-meta.cjs ───────────────────────────────────────────────────────────────────────────
const metaCases = [];
/** One call; `port` args for a named difference. */
function metaCase(fn, args, { diff = null, portArgs = null } = {}) {
  const a = wire(args);
  const row = { fn, args: a, ...result(() => meta[fn](...dec(a))) };
  if (diff) {
    const p = wire(portArgs);
    Object.assign(row, { diff, portArgs: p, port: result(() => meta[fn](...dec(p))) });
  }
  metaCases.push(row);
}

// Values a reader is handed at a numeric key. `coerced` are the ones Number() turns into a number
// (or a throw) although they are not numbers: difference D1.
const NUMERIC = [5, 0, 7.5, -1, -0, 1e21, 2 ** 53 + 1, Number.MAX_VALUE, Number.MIN_VALUE, NaN, Infinity, -Infinity,
  '12', ' 7 ', ' ﻿9 ', '　3　', '0x1f', '0X1F', '0o17', '0b101', '0x', '-0x10', '+4', '-3', '-0', '1e3', '1E-2',
  '.5', '5.', '.', '1e', '1_000', 'Infinity', '-Infinity', '+Infinity', 'infinity', 'NaN', '\u00851', '١', 'abc', '12px',
  '0x' + 'f'.repeat(20), '1'.repeat(400), '0b' + '1'.repeat(70), '00012', '1e400', '-1e400'];
const COERCED = [null, true, false, '', '   ', '\t\n', [], [7], ['7'], [[8]], [1, 2], [null], {}, { valueOf: 3 }, { toString: 'x' },
  [{ toString: 1 }]];
const isCoerced = (v) => !(typeof v === 'number' || (typeof v === 'string' && v.trim() !== '') || v === undefined);
/** `make(v)` with v from both lists; a coerced value is the D1 difference, defined by `make(undefined)`. */
function numericGrid(fn, make, values = [...NUMERIC, ...COERCED]) {
  for (const v of values) {
    if (isCoerced(v)) metaCase(fn, make(v), { diff: 'D1', portArgs: make(undefined) });
    else metaCase(fn, make(v));
  }
}

const INPUT = ['inputTokens', 'input_tokens', 'promptTokens', 'prompt_tokens', 'input', 'prompt'];
const OUTPUT = ['outputTokens', 'output_tokens', 'completionTokens', 'completion_tokens', 'output', 'completion'];
const TOTAL = ['totalTokens', 'total_tokens', 'total'];
const HOLD = [(h) => ({ usage: h }), (h) => ({ tokenUsage: h }), (h) => ({ tokens: h }), (h) => ({ noevia: { usage: h } }), (h) => h];

for (const bad of [undefined, null, 0, 1, 'usage', true, [], [{ inputTokens: 1 }], {}, { usage: null }, { usage: 'x' }, { noevia: 'x' }]) metaCase('readUsage', [bad]);
for (const hold of HOLD) {
  for (const k of INPUT) numericGrid('readUsage', (v) => [hold({ [k]: v, outputTokens: 4 })], [5, '12', -1, NaN, null, '', true]);
  for (const k of OUTPUT) numericGrid('readUsage', (v) => [hold({ inputTokens: 3, [k]: v })], [5, '12', -1, NaN, null, '', true]);
  for (const k of TOTAL) numericGrid('readUsage', (v) => [hold({ input: 1, output: 2, [k]: v })], [5, '12', -1, Infinity, null]);
}
numericGrid('readUsage', (v) => [{ usage: { inputTokens: v } }]);
numericGrid('readUsage', (v) => [{ usage: { inputTokens: 2, outputTokens: v } }]);
numericGrid('readUsage', (v) => [{ usage: { inputTokens: 2, outputTokens: 3, totalTokens: v } }]);
// Precedence: the first holder that has a number wins, the first key in each list wins.
metaCase('readUsage', [{ usage: { total: 9 }, tokenUsage: { input: 1 } }]);
metaCase('readUsage', [{ usage: { inputTokens: 'x' }, tokens: { prompt: 4, completion: 5 }, inputTokens: 9 }]);
metaCase('readUsage', [{ usage: { inputTokens: 1, input_tokens: 2, input: 3, outputTokens: 4, output: 5 } }]);
metaCase('readUsage', [{ usage: { input_tokens: -1, prompt_tokens: 6, completion: '2' } }]);
metaCase('readUsage', [{ usage: [1, 2], noevia: { usage: { input: 1 } }, output: 7 }]);
metaCase('readUsage', [{ usage: { inputTokens: 1e308, outputTokens: 1e308 } }]);
metaCase('readUsage', [{ usage: { inputTokens: 0.1, outputTokens: 0.2 } }]);
metaCase('readUsage', [{ usage: { inputTokens: 1, outputTokens: 2, total_tokens: 'many', total: 4 } }]);
metaCase('readUsage', [J('{"__proto__":{"inputTokens":1},"output":2}')]);
metaCase('readUsage', [J('{"usage":{"__proto__":5,"input":1}}')]);

const USED = ['used', 'usedTokens', 'used_tokens', 'contextUsed', 'context_used'];
const SIZE = ['size', 'contextSize', 'context_size', 'limit', 'window'];
const CHOLD = [(h) => h, (h) => ({ usage: h }), (h) => ({ _meta: h }), (h) => ({ context: h })];
for (const bad of [undefined, null, 5, 'used', false, [], {}, { usage: 3 }, { context: [] }]) metaCase('readContext', [bad]);
for (const hold of CHOLD) {
  for (const u of USED) metaCase('readContext', [hold({ [u]: 8012, size: 24576 })]);
  for (const s of SIZE) metaCase('readContext', [hold({ used: 8012, [s]: 24576 })]);
}
numericGrid('readContext', (v) => [{ used: v, size: 200 }]);
numericGrid('readContext', (v) => [{ used: 50, size: v }]);
for (const [used, size] of [[0, 1], [1, 3], [2, 3], [1, 8], [5, 1000], [15, 1000], [25, 1000], [1, 200], [3, 200], [1, 0.5],
  [1e308, 1e-300], [0.49999999999999994, 100], [7, 7], [8, 7], [1, 1e300], ['5', '10'], [-0, 4], [0, 0], [3, 0], [0, -1]]) {
  metaCase('readContext', [{ used, size }]);
}
metaCase('readContext', [{ used: 1, usage: { used: 2, size: 4 } }]);
metaCase('readContext', [{ used: 1, size: 0, _meta: { used: 3, size: 6 }, context: { used: 1, size: 2 } }]);
metaCase('readContext', [{ usedTokens: 'x', used_tokens: 3, contextSize: -2, limit: 9 }]);
metaCase('readContext', [{ sessionUpdate: 'usage_update', used: 8012, size: 24576, cost: { amount: 0, currency: 'USD' } }]);

const EXIT_KEYS = ['exitCode', 'exit_code', 'exitStatus', 'status_code', 'code'];
const EHOLD = [(h) => h, (h) => ({ _meta: h }), (h) => ({ meta: h }), (h) => ({ rawOutput: h }), (h) => ({ _meta: { noevia: h } })];
for (const bad of [undefined, null, 0, 'exitCode', true, [], {}, { _meta: 'x' }, { rawOutput: [0] }]) metaCase('readExitCode', [bad]);
for (const hold of EHOLD) for (const k of EXIT_KEYS) {
  for (const v of [0, 1, 127, -1, '3', NaN, 2.5]) metaCase('readExitCode', [hold({ [k]: v })]);
}
const EXIT_VALUES = [0, 1, -1, -0, 255, 2.5, 1e21, 2 ** 53 + 2, NaN, Infinity, '3', '0', '-0', ' 4 ', '0x10', '1e2', '2.0', '2.5', 'x', 'ok'];
for (const v of [...EXIT_VALUES, ...COERCED]) {
  const make = (x) => [{ exitCode: x, rawOutput: { code: 9 } }];
  if (isCoerced(v)) metaCase('readExitCode', make(v), { diff: 'D1', portArgs: make(undefined) });
  else metaCase('readExitCode', make(v));
}
metaCase('readExitCode', [{ exitCode: 'x', exit_code: 2, _meta: { exitCode: 3 } }]);
metaCase('readExitCode', [{ _meta: { code: 'x' }, meta: { code: 4 }, rawOutput: { code: 5 } }]);
metaCase('readExitCode', [{ _meta: { noevia: { exitStatus: 6 } }, rawOutput: { status_code: 'n' } }]);
metaCase('readExitCode', [{ status: 'completed', rawOutput: { output: '', exitCode: 0 } }]);

for (const r of [undefined, null, 5, 'agentInfo', [], {}, { agentInfo: null }, { agentInfo: 'opencode' }, { agentInfo: [] },
  { agentInfo: { name: 'opencode', version: '1.18.31' }, protocolVersion: 1 },
  { serverInfo: { name: 'pi', version: '0.87.0' }, protocolVersion: '1' },
  { _meta: { agentInfo: { name: 'claude-code', version: 2 } } },
  { agentInfo: {}, serverInfo: { name: 'second' } },
  { agentInfo: '', serverInfo: { name: 'second' } },
  { agentInfo: 0, serverInfo: 0, _meta: { agentInfo: { name: 'third' } } },
  { agentInfo: { name: 42, version: [] } },
  { agentInfo: { name: '', version: '' } },
  { agentInfo: { name: 'x'.repeat(200), version: 'v'.repeat(100) } },
  { agentInfo: { name: 'n'.repeat(79) + '\u{1F600}', version: 'v'.repeat(39) + '\u{1F600}x' } },
  { agentInfo: { name: '\ud800' + 'a'.repeat(90), version: '\udfff' } },
  { agentInfo: { name: 'é'.repeat(81) } },
]) metaCase('readAgent', [r]);
numericGrid('readAgent', (v) => [{ agentInfo: { name: 'a' }, protocolVersion: v }]);

const BASE = { harness: 'opencode', harnessVersion: '1.18.31', model: 'qa-coder-7b', provider: 'llama.cpp', protocolVersion: 1,
  capabilities: ['edit_file', 'execute_command'], promptPreparation: 'direct', sandbox: 'sandbox' };
for (const a of [[], [undefined], [{}], [BASE], [{ ...BASE, capabilities: ['git_push', 'edit_file'] }],
  [{ harness: '' }], [{ harness: 0, sandbox: '' }], [{ harness: null, sandbox: null, harnessVersion: undefined, promptPreparation: undefined }],
  [{ harness: 'x', harnessVersion: null, model: null, provider: null, protocolVersion: null, promptPreparation: null }],
  [{ harness: 'x', capabilities: [10, 9, 1, null, '\u{1F600}', '￿', [2, 1], true, 'B', 'a', 'Z', '_'] }],
  [{ harness: 'x', capabilities: ['b', undefined, 'a', undefined] }],
  [{ harness: 'x', capabilities: [{ toString: 1 }] }],
  [{ harness: 'x', capabilities: [{ toString: 1 }, undefined] }],
  [{ harness: 'x', capabilities: [{ toString: 1 }, 'a'] }],
  [{ harness: 'x', capabilities: [[{ toString: 1 }], 'a'] }],
  [{ harness: 'x', capabilities: 'push\u{1F600}\ud800a' }],
  [{ harness: 'x', capabilities: '' }],
  [{ harness: 'x', capabilities: null }],
  [{ harness: 'x', capabilities: 5 }],
  [{ harness: 'x', capabilities: {} }],
  [{ harness: 'x', capabilities: { 0: 'a', length: 1 } }],
  [null],
  [5], ['opencode'], [[]], [true],
  [{ harness: 'x', model: { b: 1, a: [{ z: 1, y: 2 }], 10: 'ten', 2: 'two', '\u{1F600}': 1, '￿': 2, A: 3 } }],
  [{ harness: 'x', provider: { nested: { c: NaN, b: -0, a: Infinity, u: undefined } } }],
  [{ harness: 'x', model: 'qa\ud83d', provider: '\udc00qa' }],
  [{ harness: 'x', model: NaN, protocolVersion: -0, sandbox: 0 }],
  [{ harness: 'x', extra: 'ignored', capabilities: ['a'] }],
  [{ harness: ['a'], harnessVersion: { v: 1 }, promptPreparation: ['split'], sandbox: { kind: 'vm' } }],
]) metaCase('codingIdentity', a);

const EXITS = Array.from({ length: 60 }, (_, i) => ({ id: `t${i}`, name: 'bash', exitCode: i % 3 }));
for (const a of [[], [undefined], [null], [5], ['x'], [[]], [{}],
  [{ agent: { name: 'opencode', version: '1.18.31', protocolVersion: 1 }, usage: { input: 5, output: 2, total: 7 },
    context: { used: 8012, size: 24576, percent: 33 }, exits: [{ id: 'a', exitCode: 0 }, { id: 'b', exitCode: 2 }], messageChunks: 50 }],
  [{ agent: { name: 'opencode', version: '1.18.31' }, usage: { input: 5, output: null, total: null } }],
  [{ agent: { name: 42, version: [] } }],
  [{ agent: { name: 'x', version: '' } }],
  [{ agent: { name: 'x', version: 0 } }],
  [{ agent: { name: 'x', version: 2, protocolVersion: '1' } }],
  [{ agent: { name: 'x', version: 'v', protocolVersion: NaN } }],
  [{ agent: { name: 'x', version: 'v', protocolVersion: -3.5 } }],
  [{ agent: null, usage: null, context: null, exits: null }],
  [{ agent: 'opencode', usage: 'lots', context: 'full', exits: 'none', messageChunks: '5' }],
  [{ agent: ['a'], usage: [1], context: [1], exits: {} }],
  [{ usage: { total: 0 } }],
  [{ usage: { total: undefined, input: 1 } }],
  [{ usage: { total: 'x' } }],
  [{ usage: { total: NaN } }],
  [{ context: { used: 1, size: 2, percent: 50, extra: true } }],
  [{ context: { used: '1', size: 2 } }],
  [{ context: { used: 1, size: Infinity } }],
  [{ context: { used: 1 } }],
  [{ exits: [null, 0, 'x', [], { exitCode: 0 }, { exitCode: 1 }, { exitCode: -0 }, { exitCode: 2.5 }, { exitCode: '3' }, { exitCode: null }, { exitCode: NaN }, { exitCode: -2 }, [1]] }],
  [{ exits: EXITS }],
  [{ messageChunks: Infinity }],
  [{ messageChunks: -2.5 }],
  [{ messageChunks: NaN }],
  [{ messageChunks: -0 }],
]) metaCase('summarize', a);

// ── code-tool-schemas.cjs ───────────────────────────────────────────────────────────────────
const schemaCases = [];
function schemaCase(fn, args, { diff = null, portArgs = null, port = null } = {}) {
  const a = wire(args);
  const call = () => {
    const out = schemas[fn](...dec(a));
    return out instanceof Error ? { error: errorRaw(out) } : out;
  };
  const row = { fn, args: a, ...result(call) };
  if (diff) {
    row.diff = diff;
    if (portArgs) {
      const p = wire(portArgs);
      row.portArgs = p;
      row.port = result(() => schemas[fn](...dec(p)));
    } else row.port = { out: enc(port) };
  }
  schemaCases.push(row);
}

const KINDS = [...schemas.ACP_KINDS];
const ROOT = '/work/tree';
// Well-formed calls from the harnesses noevia runs, and the core tests' malformed ones.
const CALLS = [
  { toolCallId: 'c1', kind: 'edit', title: `${ROOT}/median.js`, locations: [{ path: `${ROOT}/median.js` }], rawInput: { filepath: `${ROOT}/median.js`, diff: '@@' } },
  { toolCallId: 'c2', kind: 'execute', title: 'node test.js', rawInput: { command: 'node test.js', cwd: ROOT } },
  { toolCallId: 'pi-bash', kind: 'execute', title: 'ls', rawInput: { command: 'ls -la' } },
  { toolCallId: 'pi-write', kind: 'edit', title: 'write', rawInput: { path: 'src/x.js', content: R('x', 100000) }, locations: [{ path: 'src/x.js' }] },
  { toolCallId: 'pi-grep', kind: 'search', title: 'grep', rawInput: { pattern: 'TODO', path: '.' } },
  { toolCallId: 'r', kind: 'read', rawInput: { filePath: `${ROOT}/a.txt` }, locations: [{ path: `${ROOT}/a.txt`, line: 3 }] },
  { kind: 'think', title: 'Thinking' },
  { kind: 'fetch', rawInput: { url: 'https://registry.example.invalid/x' } },
  { kind: 'execute', rawInput: { command: ['npm', 'test'] } },
  { kind: 'other', title: 'todowrite', rawInput: { todos: [{ content: 'x' }] } },
  { toolCallId: 'only-id' },
  { kind: 'execute', rawInput: {} },
  { kind: 'execute' },
  { kind: 'execute', rawInput: { command: 42 } },
  { kind: 'execute', rawInput: { command: 'ls', cwd: '' } },
  { kind: 'execute', rawInput: 'rm -rf /' },
  { kind: 'edit', title: 'Edit', rawInput: { content: 'x' } },
  { kind: 'edit', locations: [{ path: 'src/a\nb.js' }] },
  { kind: 'edit', locations: [{ line: 1 }] },
  { kind: 'edit', locations: 'a.txt' },
  { kind: 'delete', rawInput: { path: '' } },
  { kind: 'move', rawInput: { path: 'a\0b' } },
  { kind: 'read', locations: [{ path: 'a\u001b[2Jb' }] },
  { kind: 'fetch', rawInput: { url: 42 } },
  { kind: 'fetch', rawInput: {} },
  { kind: 'launch_missiles', rawInput: { command: 'ls' } },
  { kind: 'think', toolCallId: 7 },
  { kind: 'other', rawInput: { path: ['a', 'b'] } },
  { kind: 'execute', rawInput: { command: R('x', 64 * 1024 + 1) } },
  { kind: 'execute', rawInput: { command: R('x', 64 * 1024) } },
  { kind: 'read', locations: [{ path: '../../outside.txt' }] },
  { kind: 'other', title: 'read outside', locations: [{ path: '/home/agent/.ssh/id_rsa' }], rawInput: { path: '/home/agent/.ssh/id_rsa', noeviaOutsideWorkspace: true } },
  { kind: 'edit', locations: [{ path: '/etc/passwd' }] },
  { kind: 'delete', rawInput: { path: '/etc/passwd' } },
  { kind: 'execute', rawInput: { command: 'ls', cwd: '/etc' } },
  { kind: 'fetch', rawInput: { url: 'file:///etc/passwd' } },
];
for (const c of CALLS) schemaCase('checkToolCall', [c]);
for (const c of [undefined, null, 5, 'execute', true, [], [{ kind: 'execute' }], {}]) schemaCase('checkToolCall', [c]);
// Every kind, with and without what it needs.
for (const kind of [...KINDS, null, undefined]) {
  schemaCase('checkToolCall', [{ kind }]);
  schemaCase('checkToolCall', [{ kind, rawInput: {} }]);
  schemaCase('checkToolCall', [{ kind, rawInput: null }]);
  schemaCase('checkToolCall', [{ kind, rawInput: [] }]);
  schemaCase('checkToolCall', [{ kind, rawInput: 'text' }]);
  schemaCase('checkToolCall', [{ kind, locations: [{ path: 'a.txt' }] }]);
  schemaCase('checkToolCall', [{ kind, rawInput: { path: 'a.txt' } }]);
  schemaCase('checkToolCall', [{ kind, rawInput: { cwd: 'a' } }]);
  schemaCase('checkToolCall', [{ kind, rawInput: { command: 'ls' } }]);
  schemaCase('checkToolCall', [{ kind, rawInput: { url: 'https://example.invalid' } }]);
  schemaCase('checkToolCall', [{ kind, toolCallId: R('i', 257), title: 't' }]);
  schemaCase('checkToolCall', [{ kind, status: R('s', 65) }]);
  schemaCase('checkToolCall', [{ kind, content: [{ type: 'diff', newText: R('d', 1000) }] }]);
  schemaCase('checkToolCall', [{ kind, content: 'text' }]);
  schemaCase('checkToolCall', [{ kind, rawInput: { x: R('y', 70000) } }]);
}
for (const kind of [42, true, {}, [], ['execute'], 'EXECUTE', 'execute ', '', 'k'.repeat(100), '\u{1F600}'.repeat(30), 'a\ud800', '__proto__', 'constructor']) {
  schemaCase('checkToolCall', [{ kind, rawInput: { command: 'ls' } }]);
}
// D2: String(kind) throws for a value with its own toString.
for (const kind of [{ toString: 1 }, [{ toString: 'x' }], { toString: 1, valueOf: 2 }]) {
  const call = { kind, rawInput: { command: 'ls' } };
  schemaCase('checkToolCall', [call], { diff: 'D2', portArgs: [dropToString(call)] });
}
// Every path position, with path shapes: where a path points is never judged.
const PATHS = ['a.txt', '../../x', '/abs/x', '.', '', ' ', '\t', '　', '﻿', ' ', ' ', '\u0085', 'a\u0000b', 'a\u001fb', 'a\u007fb',
  'a\u0080b', 'tab\there', 'émoji\u{1F600}', 'lone\ud800', R('p', 4096), R('p', 4097), 5, null, ['a'], { p: 1 }, true];
for (const p of PATHS) {
  schemaCase('checkToolCall', [{ kind: 'edit', locations: [{ path: p }] }]);
  schemaCase('checkToolCall', [{ kind: 'read', locations: [{ path: 'ok' }, { path: p, line: 2 }] }]);
  for (const key of schemas.PATH_KEYS) schemaCase('checkToolCall', [{ kind: 'delete', rawInput: { [key]: p } }]);
  schemaCase('checkToolCall', [{ kind: 'execute', rawInput: { command: 'ls', cwd: p } }]);
  schemaCase('checkToolCall', [{ kind: 'search', rawInput: { cwd: p } }]);
  schemaCase('checkToolCall', [{ kind: 'think', rawInput: { path: p } }]);
}
// Locations.
for (const locations of [[], null, {}, [null], ['a'], [[]], [{ path: 'a', line: null }], [{ path: 'a', line: 1.5 }], [{ path: 'a', line: '1' }],
  [{ path: 'a', line: -1 }], [{ path: 'a', line: 0 }], [{ path: 'a', extra: { deep: [1, 2] } }], Array.from({ length: 64 }, () => ({ path: 'a' })),
  Array.from({ length: 65 }, () => ({ path: 'a' })), [{ path: 'ok' }, { path: '' }], [{ path: 'b' }, 5]]) {
  schemaCase('checkToolCall', [{ kind: 'move', locations }]);
}
// Commands, under every key the command reader takes.
const COMMANDS = ['ls', '  ls  ', '', '   ', 'a\u0000b', [], ['ls'], ['', ''], ['  ', ' '], ['a', 'b\u0000'], [1], [null], [true], ['a', {}],
  R('c', 65536), [R('c', 60000), R('c', 60000), R('c', 60000), R('c', 60000), R('c', 60000)],
  [R('c', 60000), R('c', 60000), R('c', 60000), R('c', 60000), 'x\u0000'], { cmd: 'x' }, 7, null, true];
for (const c of COMMANDS) {
  for (const key of ['command', 'cmd', 'script', 'shell', 'commandLine', 'args']) {
    schemaCase('checkToolCall', [{ kind: 'execute', rawInput: { [key]: c } }]);
  }
  schemaCase('checkToolCall', [{ kind: 'execute', rawInput: { command: '', args: c } }]);
  schemaCase('checkToolCall', [{ kind: 'fetch', rawInput: { command: c } }]);
  schemaCase('checkToolCall', [{ kind: 'fetch', rawInput: { url: ' ', script: c } }]);
}
schemaCase('checkToolCall', [{ kind: 'execute', rawInput: { command: 'git push', noeviaOutsideWorkspace: true } }]);
schemaCase('checkToolCall', [{ kind: 'execute', rawInput: { command: 'echo "unterminated' } }]);
schemaCase('checkToolCall', [{ kind: 'execute', rawInput: { command: 'cat <<EOF\nx\nEOF' } }]);
schemaCase('checkToolCall', [{ kind: 'execute', rawInput: { command: '$(' + '('.repeat(500) } }]);
for (const url of ['', ' ', '　', 'https://example.invalid', R('u', 8192), R('u', 8193), 5, null, ['x']]) {
  schemaCase('checkToolCall', [{ kind: 'fetch', rawInput: { url } }]);
}
schemaCase('checkToolCall', [{ kind: 'think', rawInput: { x: JSON.parse('['.repeat(70) + ']'.repeat(70)) } }]);
schemaCase('checkToolCall', [{ kind: 'edit', locations: [{ path: 'a', ['k'.repeat(400)]: JSON.parse('['.repeat(70) + ']'.repeat(70)) }] }]);
schemaCase('checkToolCall', [{ kind: 'edit', locations: [{ path: 'a', ['\u{1F600}'.repeat(200)]: JSON.parse('['.repeat(70) + ']'.repeat(70)) }] }]);
schemaCase('checkToolCall', [{ kind: 'edit', locations: [{ path: 'a\ud800' }], title: '\udc00' }]);
schemaCase('checkToolCall', [{ kind: 'edit', rawInput: { path: 'a', x: NaN }, title: undefined, status: null }]);
schemaCase('checkToolCall', [{ kind: 'edit', rawInput: { path: 'a' }, title: NaN }]);
schemaCase('checkToolCall', [J('{"kind":"edit","__proto__":{"x":1},"rawInput":{"__proto__":5,"path":"a"}}')]);
schemaCase('checkToolCall', [{ kind: 'think', rawInput: R('z', 10) }]);
schemaCase('checkToolCall', [{ kind: 'think', rawInput: { big: R('z', 2 * 1024 * 1024) } }]);

// fs/* params.
for (const [method, params] of [
  ['fs/read_text_file', { sessionId: 's', path: `${ROOT}/a.txt`, line: 2, limit: 1 }],
  ['fs/write_text_file', { sessionId: 's', path: 'b.txt', content: 'hello' }],
  ['fs/read_text_file', {}], ['fs/read_text_file', null], ['fs/read_text_file', undefined], ['fs/read_text_file', 'a.txt'], ['fs/read_text_file', []],
  ['fs/read_text_file', { path: 5 }], ['fs/read_text_file', { path: '' }], ['fs/read_text_file', { path: '  ' }], ['fs/read_text_file', { path: 'a\u0000.txt' }],
  ['fs/read_text_file', { path: 'a.txt', line: 0 }], ['fs/read_text_file', { path: 'a.txt', line: -3 }], ['fs/read_text_file', { path: 'a.txt', line: 1 }],
  ['fs/read_text_file', { path: 'a.txt', line: null, limit: null }], ['fs/read_text_file', { path: 'a.txt', limit: 2.5 }], ['fs/read_text_file', { path: 'a.txt', limit: '2' }],
  ['fs/read_text_file', { path: 'a.txt', limit: 0, line: 0 }], ['fs/read_text_file', { path: 'a.txt', line: 1e21 }], ['fs/read_text_file', { path: 'a.txt', line: -0 }],
  ['fs/read_text_file', { path: 'a.txt', sessionId: 5 }], ['fs/read_text_file', { path: 'a.txt', extra: JSON.parse('['.repeat(70) + ']'.repeat(70)) }],
  ['fs/read_text_file', { path: '../../etc/shadow' }], ['fs/read_text_file', { path: '/etc/shadow', content: 'x' }], ['fs/read_text_file', { path: '/x', content: 5 }],
  ['fs/read_text_file', { path: R('p', 4097) }], ['fs/read_text_file', { line: 0 }],
  ['fs/write_text_file', { path: 'b.txt' }], ['fs/write_text_file', { content: 'x' }], ['fs/write_text_file', { path: 'b.txt', content: { text: 'x' } }],
  ['fs/write_text_file', { path: 'b.txt', content: null }], ['fs/write_text_file', { path: 'a\tb.txt', content: 'x' }], ['fs/write_text_file', { path: '', content: 'x' }],
  ['fs/write_text_file', { path: '../escape.txt', content: 'x' }], ['fs/write_text_file', { content: 'x', path: 5, sessionId: 7 }],
  ['fs/write_text_file', { path: 'b', content: 'x', extra: JSON.parse('['.repeat(70) + ']'.repeat(70)) }],
  ['fs/write_text_file', { path: 'b', content: 'x', line: 0 }],
  // A write's schema does not name line or limit, so `params[key] < 1` compares whatever came.
  ...[0, '0', ' ', '', 'a', [], [[0]], [2], false, true, {}, { toString: 1 }, { valueOf: 0 }, null, -0, 0.5, '0x0', NaN, -Infinity]
    .flatMap((v) => [['fs/write_text_file', { path: 'b', content: 'x', line: v }], ['fs/write_text_file', { path: 'b', content: 'x', limit: v }]]),
  ['fs/write_text_file', { path: 'b', content: 'x', line: [{ toString: 1 }] }],
  ['fs/write_text_file', { path: 'b', content: 'x', line: { toString: 1, valueOf: 0 } }],
  ['fs/write_text_file', { path: 'b', content: R('x', 3 * 1024 * 1024) }],
  ['fs/write_text_file', { path: 'b', content: '', _meta: { big: R('m', 2 * 1024 * 1024) } }],
  ['fs/unknown', { path: 'a' }], ['', { path: 'a' }], ['FS/READ_TEXT_FILE', { path: 'a' }],
]) {
  // D2 again: `params[key] < 1` converts the value to a primitive and throws for an own toString.
  if (hasToString(params)) schemaCase('checkFsCall', [method, params], { diff: 'D2', portArgs: [method, dropToString(params)] });
  else schemaCase('checkFsCall', [method, params]);
}
for (const maxBytes of [16, 0, 5, 3, 4, Infinity, NaN, -1, 2.5, 1e21, undefined]) {
  for (const content of ['', 'hello', 'x'.repeat(17), 'x'.repeat(16), '€', '\u{1F600}', '\ud800', '\ud800\ud800', 'é'.repeat(3)]) {
    schemaCase('checkFsCall', ['fs/write_text_file', { path: 'b.txt', content }, { maxBytes }]);
  }
}
schemaCase('checkFsCall', ['fs/write_text_file', { path: 'b', content: 'xx' }, {}]);
schemaCase('checkFsCall', ['fs/write_text_file', { path: 'b', content: 'xx' }, undefined]);
schemaCase('checkFsCall', ['fs/read_text_file', { path: 'b', content: 'xx' }, { maxBytes: 1 }]);
// D3: method and kind names Node finds on Object.prototype.
for (const method of ['constructor', 'toString', 'hasOwnProperty', '__proto__', 'valueOf']) {
  schemaCase('checkFsCall', [method, { path: 'a' }], { diff: 'D3', port: { message: `Unknown file method ${method}`, path: '$', kind: method, hint: '' } });
}

for (const kind of [...KINDS, 'unknown', '']) schemaCase('toolCallSchema', [kind]);
for (const kind of ['constructor', 'toString', '__proto__', 'hasOwnProperty']) {
  schemaCase('toolCallSchema', [kind], { diff: 'D3', port: schemas.toolCallSchema('no such kind') });
}

for (const found of [{ message: 'Missing command at $.rawInput.command', path: '$.rawInput.command' }, { message: 'm', path: '' }, { message: 'm', path: null },
  { message: 'm' }, { path: '$.x' }, {}, null, undefined, 5, 'text', { message: 5, path: '$' }, { message: null, path: '$' }, { message: undefined, path: '$' },
  { message: ['a', 'b'], path: '$' }, { message: { toString: 1 }, path: '$' }, { message: {}, path: '$' }, { message: 'm', path: 5 }, { message: 'm', path: ['$'] },
  { message: 'm', path: true }, { message: 'x'.repeat(400), path: '$' }, { message: 'x'.repeat(299) + '\u{1F600}', path: '$' },
  { message: 'x'.repeat(300), path: '$.a' }, { message: '\ud800', path: '\udc00' }, { message: 'm', path: R('p', 1024 * 1024 + 1) },
  { message: R('m', 1024 * 1024 + 5), path: '$' }, { message: '', path: '' }]) {
  schemaCase('correctionFor', [found]);
}

const V1 = { type: 'schema_violation_correction', violation: { message: 'Missing command at $.rawInput.command', path: '$.rawInput.command' },
  tool: 'session/request_permission', kind: 'execute', hint: 'Send the shell command as one non-empty string in rawInput.command.', count: 1, limit: 3, blocked: false };
const VB = { ...V1, count: 3, blocked: true };
const VIOLATIONS = [V1, VB, { ...V1, blocked: 1 }, { ...V1, blocked: '' }, { ...V1, count: undefined, limit: null }, { ...V1, count: [1, 2], limit: { a: 1 } },
  { ...V1, hint: undefined }, { ...V1, violation: { message: undefined } }, { ...V1, violation: 'x' }, { ...V1, violation: null }, { ...V1, violation: undefined },
  { violation: { message: 'm' } }, {}, null, undefined, 5, true, [], { ...V1, count: { toString: 1 } }, { ...V1, blocked: true, limit: { toString: 1 } },
  { ...V1, violation: { message: { toString: 1 } } }, { ...V1, hint: { toString: 1 } }, { ...V1, count: NaN, limit: Infinity }];
for (const v of VIOLATIONS) {
  schemaCase('violationText', [v]);
  schemaCase('violationError', [v]);
}
const SELECTED = [{ outcome: 'selected', optionId: 'n' }, { outcome: 'cancelled' }, null, undefined, 'ab', '\u{1F600}', ['x', 'y'], 5, true, {},
  { _meta: 1, a: 2 }, { a: 1, _meta: { noevia: { x: 1 } }, b: 2 }, J('{"2":"b","1":"a","_meta":0,"z":1}'), J('{"__proto__":{"x":1},"outcome":"selected"}'), [undefined]];
for (const s of SELECTED) for (const v of ['The person refused this kind of action for this task.', '', V1, VB]) schemaCase('rejectOutcome', [s, v]);
for (const v of VIOLATIONS) schemaCase('rejectOutcome', [{ outcome: 'selected', optionId: 'n' }, v]);
schemaCase('rejectOutcome', [{ outcome: 'selected', optionId: 'n' }]);

// ── The guard ───────────────────────────────────────────────────────────────────────────────
const GOOD = { kind: 'execute', rawInput: { command: 'ls' } };
const BAD = { kind: 'execute', rawInput: {} };
const P = (call, ...rest) => ({ op: 'permission', args: [call, ...rest] });
const RD = (params) => ({ op: 'readTextFile', args: [params] });
const WR = (params) => ({ op: 'writeTextFile', args: [params] });
const BE = { op: 'blockedError', args: [] };
const ST = { op: 'state', args: [] };

function makeThrown(spec) { return 'error' in spec ? Error(spec.error) : dec(spec.value); }
function playGuard(options, throwsAt, steps) {
  const trace = [];
  const seen = { event: 0, log: 0, onBlocked: 0 };
  const hit = (cb) => {
    const n = seen[cb]++;
    const t = throwsAt.find((x) => x.cb === cb && x.at === n);
    if (t) { trace[trace.length - 1].threw = true; throw makeThrown(t.thrown); }
  };
  const guard = schemas.createExecutorGuard({
    event: (type, data) => { trace.push({ cb: 'event', type, data: enc(data) }); hit('event'); },
    log: (entry) => { trace.push({ cb: 'log', entry: enc(entry) }); hit('log'); },
    onBlocked: () => { trace.push({ cb: 'onBlocked' }); hit('onBlocked'); },
    ...dec(options),
  });
  steps.forEach((s, i) => {
    let row;
    try {
      if (s.op === 'state') row = { out: { violations: guard.violations, blocked: guard.blocked } };
      else {
        const out = guard[s.op](...dec(s.args));
        row = { out: enc(out instanceof Error ? { error: errorRaw(out) } : out) };
      }
    } catch (e) { row = { throws: thrown(e) }; }
    trace.push({ step: i, ...row });
  });
  return trace;
}
const guardCases = [];
function guardCase(name, { options = {}, throwsAt = [], steps, diff = null, portSteps = null }) {
  const o = wire(options);
  const st = steps.map((s) => ({ op: s.op, args: wire(s.args) }));
  const row = { name, options: o, throwsAt: wire(throwsAt), steps: st, trace: playGuard(o, wire(throwsAt), st) };
  if (diff) {
    const ps = portSteps.map((s) => ({ op: s.op, args: wire(s.args) }));
    Object.assign(row, { diff, portSteps: ps, portTrace: playGuard(o, wire(throwsAt), ps) });
  }
  guardCases.push(row);
}

const FULL = [P(GOOD), RD({ path: '/etc/hosts' }), WR({ path: 'w', content: 'x' }), ST, BE,
  P(BAD, 'execute_command'), ST, RD({ path: 5 }), ST, WR({ path: 'x' }), ST, BE,
  P(GOOD), RD({ path: 'ok' }), WR({ path: 'ok', content: 'x' }), P(BAD), ST, BE];
guardCase('the core sequence: two passes, three strikes, blocked, refused after', { steps: FULL });
for (const limit of [1, 0, 2, 5, 2.5, -1, Infinity, NaN, undefined]) {
  guardCase(`limit ${String(limit)}`, { options: { limit }, steps: [P(BAD), ST, RD({}), ST, WR({ path: '' }), ST, P({ kind: 'nope' }), ST, P(BAD, null), RD(null), WR(5), ST, BE, P(GOOD), ST, BE] });
}
for (const maxBytes of [16, 0, 3, Infinity, NaN, undefined]) {
  guardCase(`maxBytes ${String(maxBytes)}`, { options: { maxBytes }, steps: [WR({ path: 'a', content: 'x'.repeat(16) }), WR({ path: 'a', content: 'x'.repeat(17) }), WR({ path: 'a', content: '€' }), WR({ path: 'a', content: '\u{1F600}' }), ST, BE] });
}
guardCase('actions as given', { steps: [P(BAD), P(BAD, undefined), P(BAD, 'network'), P(BAD, { a: 1 }), P(BAD, 'x'), ST] });
guardCase('every kind of malformed call', { options: { limit: 100 }, steps: [
  P(null), P('x'), P({ kind: 5 }), P({ kind: 'edit' }), P({ kind: 'edit', locations: [{ path: '' }] }), P({ kind: 'read', rawInput: { path: 'a\u0000' } }),
  P({ kind: 'fetch', rawInput: {} }), P({ kind: 'execute', rawInput: { command: 'a\u0000b' } }), P({ kind: 'think', toolCallId: 7 }),
  RD(undefined), RD({ path: 'a', line: 0 }), RD({ path: '\t' }), WR({ path: 'a' }), WR({ path: 'a', content: 5 }), WR({ path: 'a\u007f', content: '' }),
  ST, BE] });
guardCase('the record failing: event throws on the denial', { throwsAt: [{ cb: 'event', at: 0, thrown: { error: 'journal down' } }], steps: [P(BAD), ST, P(BAD), ST] });
guardCase('event throws a long message', { throwsAt: [{ cb: 'event', at: 0, thrown: { error: 'e'.repeat(400) } }], steps: [RD({}), ST] });
guardCase('event throws an Error with no message', { throwsAt: [{ cb: 'event', at: 0, thrown: { error: '' } }], steps: [RD({}), ST] });
for (const value of ['text', null, undefined, 0, 42, { message: 'object message' }, { message: '' }, { message: 7 }, ['a', 'b'], {}, { toString: 1 }, { message: { toString: 1 } }]) {
  guardCase(`event throws ${JSON.stringify(enc(value))}`, { throwsAt: [{ cb: 'event', at: 0, thrown: { value } }], steps: [WR({ path: 'a' }), ST, P(BAD), ST] });
}
guardCase('step events failing at the block', { options: { limit: 1 }, throwsAt: [{ cb: 'event', at: 1, thrown: { error: 'step store down' } }], steps: [P(BAD), ST, BE] });
guardCase('step.completed failing at the block', { options: { limit: 1 }, throwsAt: [{ cb: 'event', at: 2, thrown: { error: 'second step down' } }], steps: [P(BAD), ST, BE] });
guardCase('onBlocked failing', { options: { limit: 2 }, throwsAt: [{ cb: 'onBlocked', at: 0, thrown: { error: 'cannot stop' } }], steps: [P(BAD), P(BAD), ST, P(GOOD), ST, BE] });
guardCase('onBlocked throwing a value', { options: { limit: 1 }, throwsAt: [{ cb: 'onBlocked', at: 0, thrown: { value: 'halt failed' } }], steps: [RD({}), ST] });
guardCase('event failing once blocked is swallowed', { options: { limit: 1 }, throwsAt: [{ cb: 'event', at: 3, thrown: { error: 'late' } }, { cb: 'event', at: 4, thrown: { value: null } }], steps: [P(BAD), P(GOOD), RD({ path: 'a' }), WR({}), ST, BE] });
guardCase('the log failing escapes the call', { throwsAt: [{ cb: 'log', at: 0, thrown: { error: 'log down' } }], steps: [P(BAD), ST, BE, P(BAD), ST] });
guardCase('the log failing in the record-failed handler', { throwsAt: [{ cb: 'event', at: 0, thrown: { error: 'journal' } }, { cb: 'log', at: 1, thrown: { error: 'log too' } }], steps: [P(BAD), ST, P(BAD), ST] });
guardCase('the log failing at the limit leaves the guard unblocked', { options: { limit: 1 }, throwsAt: [{ cb: 'log', at: 0, thrown: { value: 'x' } }], steps: [P(BAD), ST, BE, P(BAD), ST, BE, P(GOOD), ST] });
guardCase('everything failing', { options: { limit: 2 }, throwsAt: [
  { cb: 'event', at: 0, thrown: { error: 'a' } }, { cb: 'event', at: 1, thrown: { error: 'b' } }, { cb: 'event', at: 2, thrown: { error: 'c' } },
  { cb: 'onBlocked', at: 0, thrown: { error: 'd' } }, { cb: 'event', at: 3, thrown: { error: 'e' } }], steps: [P(BAD), WR({}), ST, RD({}), ST, BE] });
guardCase('D2: a kind whose String() throws', { options: { limit: 2 }, diff: 'D2',
  steps: [P({ kind: { toString: 1 } }), ST, WR({ path: 'a', content: 'x', limit: { toString: 1 } }), ST, P({ kind: [{ toString: 1 }] }), ST, BE],
  portSteps: [P({ kind: {} }), ST, WR({ path: 'a', content: 'x', limit: {} }), ST, P({ kind: [{}] }), ST, BE] });

// ── The deployment flag ─────────────────────────────────────────────────────────────────────
const flag = [];
flag.push({ install: null, enabled: schemas.executorGuardFlag.enabled() });
for (const spec of [{ returns: true }, { returns: 'yes' }, { returns: 1 }, { returns: false }, { returns: null }, { returns: undefined }, { returns: {} },
  { throws: { error: 'no store' } }, { throws: { value: 'x' } }, { returns: true }]) {
  const s = wire(spec);
  schemas.useExecutorGuard('throws' in s ? () => { throw makeThrown(s.throws); } : () => dec(s.returns));
  flag.push({ install: s, enabled: schemas.executorGuardFlag.enabled() });
}
let notFunction;
try { schemas.useExecutorGuard(true); } catch (e) { notFunction = thrown(e); }
flag.push({ installNotFunction: notFunction, enabled: schemas.executorGuardFlag.enabled() });
schemas.useExecutorGuard(() => false);

// ── Out ─────────────────────────────────────────────────────────────────────────────────────
const constants = {
  MAX_VIOLATIONS: schemas.MAX_VIOLATIONS, ACP_KINDS: schemas.ACP_KINDS, PATH_KEYS: schemas.PATH_KEYS,
  RAW_INPUT_SCHEMAS: schemas.RAW_INPUT_SCHEMAS, FS_SCHEMAS: schemas.FS_SCHEMAS, exports: { meta: Object.keys(meta), schemas: Object.keys(schemas) },
};
const lines = (rows) => rows.map((r) => JSON.stringify(r)).join(',\n');
process.stdout.write(`{"version":1,\n"source":${JSON.stringify({ 'code-meta.cjs': sha(metaFile), 'code-tool-schemas.cjs': sha(schemasFile) })},\n`
  + `"constants":${JSON.stringify(enc(constants))},\n`
  + `"meta":[\n${lines(metaCases)}\n],\n"schemas":[\n${lines(schemaCases)}\n],\n"guards":[\n${lines(guardCases)}\n],\n"flag":[\n${lines(flag)}\n]}\n`);
process.stderr.write(`meta ${metaCases.length}, schemas ${schemaCases.length}, guards ${guardCases.length}, flag ${flag.length}\n`);
