#!/usr/bin/env node
'use strict';
// Records tests/fixtures/host-node.v1.json from noevia-core's server/code-actions.cjs (the host side
// of the code-actions crate, ported as `code_actions::host`): what classify, decide, pickOption,
// commandOf, classifyInput, decideInput and pickInput return (or that they throw) for a synthetic
// grid, through the real dav-parse.wasm the core checkout pins.
//
//   node crates/code-actions/tools/gen-host-fixtures.cjs <noevia-core>/server
//
// Inputs are stored compactly and expanded the same way by tests/host.rs:
//   {"$repeat": [text, n]}  text repeated n times
//   {"$concat": [a, b, …]}  the (expanded) texts joined
//   {"$fill": [value, n]}   an array of n copies of value
//   {"$undefined": true}    undefined (an argument left out, or a property that is not there)
//   {"$classify": call}     decide only: classify(call) of the same module
// `inject` replaces the crate's answers, call by call: a string is a refusal with that reason (the
// loader throws it, as for the crate's `{"error": reason}`), null is the real module. Every input
// is JSON-round-tripped before it is run, so the test sees exactly what Node saw. An answer is
// `want` (JSON.stringify of it, null for undefined), `wantRuns` (the same, folded; see runs()) or
// `throws` (the error's constructor name, and its `reason` when it has one). A row where the port
// answers otherwise, by design, names the `difference` (D1, D2) and holds the port's answer.

const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

const server = process.argv[2];
if (!server) {
  console.error('usage: gen-host-fixtures.cjs <noevia-core>/server');
  process.exit(2);
}
const modPath = path.resolve(server, 'code-actions.cjs');
const lockPath = path.resolve(server, 'dav-parse.lock');
const sha = (p) => crypto.createHash('sha256').update(fs.readFileSync(p)).digest('hex');
const lock = fs.readFileSync(lockPath, 'utf8');
const ref = (lock.match(/^NOEVIA_RS_REF=(\S+)/m) || [])[1] || '';
const wasmSha = (lock.match(/^DAV_PARSE_WASM_SHA256=(\S+)/m) || [])[1] || '';

console.warn = () => {}; // portWarn: counted and stateful, not part of the answers
const ca = require(modPath);
const loader = require(path.resolve(server, 'dav-parse-wasm.cjs'));

// --- compact inputs ---------------------------------------------------------------------------
const R = (text, n) => ({ $repeat: [text, n] });
const F = (value, n) => ({ $fill: [value, n] });
const U = { $undefined: true };
const C = (call) => ({ $classify: call });
const MARKERS = ['$repeat', '$concat', '$fill', '$undefined', '$classify'];
function expand(v) {
  if (Array.isArray(v)) return v.map(expand);
  if (v && typeof v === 'object') {
    const keys = Object.keys(v);
    if (keys.length === 1 && MARKERS.includes(keys[0])) {
      const [k] = keys;
      if (k === '$repeat') return String(v.$repeat[0]).repeat(v.$repeat[1]);
      if (k === '$concat') return v.$concat.map(expand).join('');
      if (k === '$fill') return Array.from({ length: v.$fill[1] }, () => expand(v.$fill[0]));
      if (k === '$undefined') return undefined;
      if (k === '$classify') return ca.classify(expand(v.$classify));
    }
    const out = {};
    for (const key of keys) out[key] = expand(v[key]);
    return out;
  }
  return v;
}

function fakeLoader(inject) {
  let i = 0;
  const step = (real) => (...args) => {
    const r = inject[Math.min(i, inject.length - 1)];
    i += 1;
    if (r === null) return real(...args);
    throw Object.assign(new Error('injected'), { reason: r });
  };
  return {
    codeActionsClassify: step(loader.codeActionsClassify),
    codeActionsDecide: step(loader.codeActionsDecide),
    codeActionsPick: step(loader.codeActionsPick),
  };
}

// A long answer (an unverified classification keeps the oversized command) as `[text, n]` runs,
// lossless: `text` repeated `n` times, concatenated. Only ASCII runs are folded, so no run boundary
// falls inside a surrogate pair.
function runs(text) {
  const out = [];
  let lit = '';
  let i = 0;
  const ascii = (s) => /^[\x00-\x7f]*$/.test(s);
  while (i < text.length) {
    let best = null;
    for (let p = 1; p <= 16 && i + p <= text.length; p += 1) {
      const unit = text.slice(i, i + p);
      if (!ascii(unit)) break;
      let n = 1;
      while (text.startsWith(unit, i + n * p)) n += 1;
      if (n * p >= 64 && (!best || n * p > best[0].length * best[1])) best = [unit, n];
    }
    if (best) {
      if (lit) out.push([lit, 1]);
      lit = '';
      out.push(best);
      i += best[0].length * best[1];
    } else {
      lit += text[i];
      i += 1;
    }
  }
  if (lit) out.push([lit, 1]);
  if (out.map(([t, n]) => t.repeat(n)).join('') !== text) throw new Error('runs are not lossless');
  return out;
}

const cases = [];
const WHY_KEYS = ['too_large', 'input', 'ambiguous'];
const FNS = {
  classify: (a, o) => ca.classify(a[0], o),
  decide: (a, o) => ca.decide(a[0], o),
  pickOption: (a, o) => ca.pickOption(a[0], a[1], o),
  commandOf: (a) => ca.commandOf(a[0]),
  classifyInput: (a) => ca.classifyInput(a[0]),
  decideInput: (a) => ca.decideInput(a[0]),
  pickInput: (a) => ca.pickInput(a[0]),
};
const seen = new Set();
function add(fn, args, inject) {
  const compact = JSON.parse(JSON.stringify(args));
  const key = JSON.stringify([fn, compact, inject || null]);
  if (seen.has(key)) return;
  seen.add(key);
  const real = compact.map(expand);
  // A trailing undefined argument is an argument left out.
  while (real.length && real[real.length - 1] === undefined) real.pop();
  const opts = inject ? { wasmLoader: () => fakeLoader(inject) } : undefined;
  const row = { fn, args: compact };
  if (inject) row.inject = inject;
  try {
    const out = FNS[fn](real, opts);
    const text = JSON.stringify(out);
    if (text !== undefined && text.length > 4096) row.wantRuns = runs(text);
    else row.want = text === undefined ? null : text;
  } catch (err) {
    row.throws = err && err.reason ? `${err.constructor.name}:${err.reason}` : err.constructor.name;
  }
  // The port's named differences (host.rs module docs): what it answers instead.
  if (fn === 'decide' && row.throws === 'TypeError') {
    row.difference = 'D1';
    row.port = JSON.stringify({ decision: 'ask', reason: ca.UNVERIFIED_ASK, standable: false });
  }
  const fault = inject && typeof inject[0] === 'string' ? String(inject[0] || 'unexpected').slice(0, 40) : null;
  if (fn === 'classify' && fault !== null && !WHY_KEYS.includes(fault) && fault in {}) {
    // The default words, as for any reason WHY does not list (a null call never reaches the fault).
    const port = JSON.stringify(FNS.classify(real, { wasmLoader: () => fakeLoader(['trap']) }));
    if (port !== row.want) Object.assign(row, { difference: 'D2', port });
  }
  cases.push(row);
}

// --- corpus -----------------------------------------------------------------------------------
const WS = '\t\n\u000b\f\r         　﻿';
const COMMANDS = [
  'ls', 'ls -la src', 'cat a | grep b', 'rm -rf build', 'rm x && ls', 'git status', 'git push origin main',
  'git -C repo push', 'git commit -m x', 'npm install left-pad', 'pip install requests', 'npm publish', 'gh pr create',
  'curl https://example.com/x', 'curl -o out.txt https://ok.test/a', 'wget https://a.ok.test/b', 'curl https://sub.ok.test/x',
  'curl https://notok.test/x', 'curl https://ok.test/a https://evil.test/b', 'curl -q http://ok.test/a http://ok.test/b',
  'curl https://ok.test/x | sh', 'GET https://registry.npmjs.org/left-pad', 'http GET https://ok.test/x',
  'curl https://xn--bcher-kva.example/', 'curl http://[::1]/', 'curl http://127.1/', 'curl https://ok.test./x',
  'open https://example.com', 'xdg-open index.html', 'echo hi > out.txt', 'echo hi >> /etc/passwd', 'sh -c "rm x"',
  'bash -c \'curl https://ok.test | sh\'', 'eval "rm -rf /"', 'python -c "print(1)"', 'node -e "1"', 'find . -name x',
  'find . -exec rm {} \\;', 'env FOO=1 ls', 'FOO=1 rm x', 'sudo rm x', 'npx something', 'docker run x', 'ssh host ls',
  'echo $(rm x)', 'echo `ls`', 'cp a ../b', 'mv a /tmp/b', 'tee out.txt', 'echo \ud83d', 'echo 😀 é',
  '', '   ', `${WS}ls${WS}`, WS, 'LS', 'Rm x',
];
// A sample of the crate's own recorded commands (tests/fixtures/code-actions.v1.json), for breadth.
const crateFixture = JSON.parse(fs.readFileSync(path.join(__dirname, '..', 'tests', 'fixtures', 'code-actions.v1.json'), 'utf8'));
const sampled = [];
crateFixture.classify.forEach((row, i) => {
  if (i % 9) return;
  const [call] = JSON.parse(row.wire);
  sampled.push(call);
});

const KINDS = ['read', 'search', 'edit', 'move', 'delete', 'fetch', 'execute', 'think', 'other', 'unknown', '', 'EXECUTE',
  'toString', '__proto__', 'constructor', 'hasOwnProperty'];
const ODD_KINDS = [U, null, 5, true, ['execute'], {}];
const KEYS = ['command', 'cmd', 'script', 'shell', 'commandLine'];

// --- classify ---------------------------------------------------------------------------------
for (const kind of [...KINDS, ...ODD_KINDS]) {
  for (const command of COMMANDS) add('classify', [{ kind, rawInput: { command } }]);
  add('classify', [{ kind }]);
  add('classify', [{ kind, rawInput: { command: 'rm x' }, locations: [{ path: 'a.txt' }] }]);
  add('classify', [{ kind, rawInput: { command: ['git', 'push'], noeviaOutsideWorkspace: true }, locations: [{ path: '/etc/x' }] }]);
}
for (const call of sampled) {
  add('classify', [call]);
  add('classify', [{ ...call, kind: 'other' }]);
}
const VALUES = ['ls -la', '  ', WS, '', ['git', 'push'], [], [' '], ['', ''], [1, 2], [-0], [0.5], [1e21], [1e-7],
  [9007199254740991], [9007199254740992], [-9007199254740991], [true, null, false], [{}], [[1]], [[]], [{ toString: 'x' }],
  [{ valueOf: 1 }], [[1, [2, 3]], null], ['a', { b: 1 }], 5, 0, true, false, null, { a: 1 }, { toString: 'x' },
  ['\ud800'], ['echo', '😀']];
for (const key of [...KEYS, 'args']) {
  for (const value of VALUES) {
    for (const kind of ['execute', 'other', 'read', 'edit']) add('classify', [{ kind, rawInput: { [key]: value } }]);
  }
}
// Several keys at once: the first that gives a command wins; an array (even of blanks) ends the search.
const MULTI = [
  { command: '  ', cmd: 'rm x' }, { command: [], script: 'ls' }, { command: [' '], cmd: 'rm x' }, { command: 5, shell: 'curl https://ok.test' },
  { cmd: 'ls', command: 'rm x' }, { commandLine: 'ls', args: ['rm', 'x'] }, { args: ['rm', 'x'] }, { args: 'rm x' },
  { args: [] }, { args: [' '] }, { command: '', args: ['ls'] }, { command: null, cmd: null, script: [{}] },
  { command: 'ls', args: [{}] }, { command: [{}], args: ['ls'] }, { shell: [1.5], command: 'ls' },
  { command: 'ls', noeviaOutsideWorkspace: true }, { command: 'ls', noeviaOutsideWorkspace: 'true' },
  { command: 'ls', noeviaOutsideWorkspace: 1 }, { command: 'ls', noeviaOutsideWorkspace: false },
  { noeviaOutsideWorkspace: true }, {}, { title: 'x', path: 'y' },
];
for (const rawInput of MULTI) {
  for (const kind of ['execute', 'edit', 'delete', 'think', U]) {
    add('classify', [{ kind, rawInput }]);
    add('classify', [{ kind, rawInput, locations: [{ path: 'a' }, { path: 'a' }, { path: '' }, { path: 5 }, {}, null, 'str', 5, { path: 'b' }] }]);
  }
}
for (const rawInput of ['ls', ['ls'], 5, null, true, '', 0, U]) {
  for (const kind of ['execute', 'edit', 'fetch']) add('classify', [{ kind, rawInput }]);
}
const LOCATIONS = [[], [{ path: 'a.txt' }], [{ path: 'a' }, { path: 'b' }, { path: 'a' }], [{ path: '' }, { path: null }],
  [null, 0, '', false, 'x', 5, true, [], { path: ['a'] }], { path: 'a' }, 'a', 5, null, [{ path: '\ud83d' }, { path: 'é' }]];
for (const locations of LOCATIONS) {
  for (const kind of ['edit', 'delete', 'read', 'execute', 'fetch']) {
    add('classify', [{ kind, rawInput: { command: 'cat a > b' }, locations }]);
    add('classify', [{ kind, title: 'Edit a.txt', rawInput: { command: [0.5] }, locations }]);
  }
}
for (const call of [U, null, 5, 'str', '', [], [{ kind: 'read' }], true, false, {}, { title: 'only a title' }]) add('classify', [call]);
// Refusals: each reason the crate gives, with the fallback's own fields varied.
const BIG = R('a', 300000); // more than MAX_WORK units of command text
const HUGE = R('ab', 1100000); // a request over 2 MiB
const HTTP = { $concat: ['curl ', R('http://', 40000)] };
const FAULTS = [
  { command: BIG }, { command: HUGE }, { cmd: [BIG] }, { args: F('x', 300000) }, { command: HTTP },
  { command: [1.5] }, { command: [{}] }, { command: [[1]] }, { args: ['ls', 1e300] }, { script: [{ toString: 'x' }] },
  { command: ['rm', 0.1] }, { shell: [{ valueOf: 1 }] },
];
for (const rawInput of FAULTS) {
  for (const kind of ['execute', 'edit', 'delete', 'fetch', 'think', 'read', 'other', 'nope', U]) {
    add('classify', [{ kind, rawInput }]);
    add('classify', [{ kind, rawInput: { ...rawInput, noeviaOutsideWorkspace: true }, locations: [{ path: 'x' }, { path: 'x' }, { path: 'y' }, null] }]);
  }
}
add('classify', [{ kind: 'edit', rawInput: { command: 'ls' }, locations: F({ path: R('p', 1000) }, 2200) }]);
add('classify', [{ kind: 'edit', rawInput: { command: 'ls' }, locations: F({ path: 'p' }, 600000) }]);

// Injected answers: every refusal reason the loader can raise, and a few it cannot.
const REASONS = ['input', 'too_large', 'ambiguous', 'reply', 'trap', 'unknown', 'unexpected', '', 'x'.repeat(50),
  `${'y'.repeat(39)}\ud83d\ude00`, 'constructor', 'toString', '__proto__', 'hasOwnProperty', 'valueOf', 'isPrototypeOf'];
for (const r of REASONS) {
  add('classify', [{ kind: 'execute', rawInput: { command: 'rm -rf x' } }], [r]);
  add('classify', [{ kind: 'edit', rawInput: { cmd: ['a', { toString: 'x' }], noeviaOutsideWorkspace: true }, locations: [{ path: 'a' }] }], [r]);
  add('classify', [{ kind: 'think' }], [r]);
  add('classify', [null], [r]);
}

// --- commandOf / classifyInput ----------------------------------------------------------------
const RAWS = [...MULTI, ...VALUES.map((v) => ({ command: v })), ...VALUES.map((v) => ({ args: v })), 'ls', ['ls'], 5, null, U, true, ''];
for (const raw of RAWS) {
  add('commandOf', [raw]);
  add('classifyInput', [{ kind: 'execute', rawInput: raw, locations: [{ path: 'a' }, null, 5] }]);
}
for (const call of [U, null, 5, 'str', [], {}, { kind: 5, locations: 'x' }, { kind: 'edit', locations: [{ path: 'a' }, { path: 1 }, 'b', null] }]) {
  add('classifyInput', [call]);
}

// --- decide -----------------------------------------------------------------------------------
const CAPS = [U, [], ['execute_command'], ['edit_file'], ['read_repository'], ['network'], ['delete'],
  ['external_account', 'execute_command', 'edit_file', 'delete', 'network', 'install_dependency', 'git_push', 'open_browser', 'read_repository'],
  null, 'execute_command', [1], ['execute_command', 5], ['']];
const DOMAINS = [U, [], ['example.com'], ['ok.test'], ['registry.npmjs.org'], null, [5]];
const INWS = [U, true, false, null, 'false', 0];
const COMBOS = [];
for (const capabilities of CAPS.slice(0, 8)) {
  for (const inWorkspace of [U, true, false, null]) COMBOS.push({ capabilities, domains: ['ok.test'], inWorkspace });
}
for (const domains of DOMAINS) COMBOS.push({ capabilities: U, domains, inWorkspace: null });
for (const capabilities of CAPS.slice(8)) COMBOS.push({ capabilities, inWorkspace: false });
for (const inWorkspace of INWS.slice(4)) COMBOS.push({ capabilities: ['edit_file'], inWorkspace });

const CLASSIFY_FOR_DECIDE = [];
for (const command of COMMANDS) {
  CLASSIFY_FOR_DECIDE.push({ kind: 'execute', rawInput: { command } });
}
for (const kind of KINDS.slice(0, 10)) {
  CLASSIFY_FOR_DECIDE.push({ kind, rawInput: { command: 'curl https://ok.test/x' }, locations: [{ path: 'a.txt' }] });
}
for (const rawInput of FAULTS) CLASSIFY_FOR_DECIDE.push({ kind: 'edit', rawInput, locations: [{ path: 'a' }] });
for (const rawInput of FAULTS.slice(5, 8)) {
  CLASSIFY_FOR_DECIDE.push({ kind: 'fetch', rawInput: { ...rawInput, noeviaOutsideWorkspace: true } });
  CLASSIFY_FOR_DECIDE.push({ kind: 'think', rawInput });
}
for (const call of sampled.filter((_, i) => i % 3 === 0)) CLASSIFY_FOR_DECIDE.push(call);
const decideClassified = (call) => {
  const c = ca.classify(expand(JSON.parse(JSON.stringify(call))));
  return JSON.stringify(c).length > 4096 ? C(call) : c;
};
CLASSIFY_FOR_DECIDE.forEach((call, i) => {
  const classified = decideClassified(call);
  const combos = i % 2 ? COMBOS.filter((_, j) => j % 2 === 0) : COMBOS.filter((_, j) => j % 2 === 1);
  for (const combo of combos) add('decide', [{ classified, ...combo }]);
});
// Hand-made classifications: the shape check, the floor, and the network branch.
const base = { action: 'execute_command', approval: 'always', command: 'ls', paths: [], readable: true, actions: ['execute_command'], simple: true };
const SHAPES = [
  base, { ...base, action: 5 }, { ...base, approval: null }, { ...base, command: [] }, { ...base, readable: 'true' },
  { ...base, simple: 1 }, { ...base, paths: ['a', 1] }, { ...base, paths: 'a' }, { ...base, actions: [1] }, { ...base, actions: U },
  { ...base, actions: 'x' }, { ...base, actions: [] }, { ...base, actions: [], action: 'edit_file', paths: ['a'] },
  { ...base, action: 'edit_file', paths: ['a'], actions: ['edit_file'], approval: 5 },
  { ...base, actions: ['edit_file', 5], paths: ['a'] }, { ...base, actions: ['delete'], paths: [] },
  { ...base, actions: ['', 'git_push'] }, { ...base, actions: ['none', 'read_repository', 'git_push'] },
  { ...base, approval: 'never' }, { ...base, approval: 'capability', command: 'curl https://ok.test/a' },
  { ...base, approval: 'capability', command: 'curl https://xn--x.test/', actions: ['network'] },
  { ...base, approval: 'capability', command: 'curl https://xn--x.test/', actions: ['network', 'edit_file'], paths: ['a'] },
  { ...base, approval: 'capability', command: 'curl http://[::1]/', simple: false },
  { ...base, approval: 'capability', command: HTTP },
  { ...base, command: HUGE }, { ...base, approval: 'capability', command: HUGE, actions: ['edit_file'], paths: ['a'] },
  { ...base, approval: 'bogus', readable: false }, { ...base, readable: false }, { ...base, standable: 'x', extra: [{}] },
  { ...base, unverified: false }, { ...base, unverified: 'true' },
  null, U, 'str', [], 5, {}, { action: 'edit_file' }, { actions: ['delete'], paths: ['x'] },
];
for (const classified of SHAPES) {
  for (const combo of COMBOS.filter((_, j) => j % 3 === 0)) add('decide', [{ classified, ...combo }]);
  add('decide', [{ classified, capabilities: ['execute_command'], domains: ['ok.test'], inWorkspace: false }]);
  add('decide', [{ classified, capabilities: ['network', 'edit_file'], domains: ['x.test'], inWorkspace: true }]);
  add('decideInput', [{ classified, capabilities: ['execute_command'], domains: [], inWorkspace: false }]);
}
// Unverified classifications, as classify gives them and as a caller might hand them in.
const unv = { action: 'external_account', approval: 'always', command: 'x', paths: [], readable: false,
  actions: ['external_account', 'execute_command'], simple: false, standable: false, unverified: true, unverifiedWhy: 'too large or complex to check' };
const UNVERIFIED = [
  unv, { ...unv, outsideWorkspace: true }, { ...unv, outsideWorkspace: 'yes' }, { ...unv, unverifiedWhy: U }, { ...unv, unverifiedWhy: '' },
  { ...unv, unverifiedWhy: 0 }, { ...unv, unverifiedWhy: [] }, { ...unv, unverifiedWhy: ['a', ['b']] }, { ...unv, unverifiedWhy: {} },
  { ...unv, unverifiedWhy: 5 }, { ...unv, unverifiedWhy: true }, { ...unv, unverifiedWhy: { toString: 'x' } }, { ...unv, unverifiedWhy: [{ toString: 1 }] },
  { ...unv, actions: U }, { ...unv, actions: [] }, { ...unv, actions: [], action: 'edit_file', paths: ['a'] },
  { ...unv, actions: ['edit_file'], paths: ['a'] }, { ...unv, actions: ['delete'], paths: 'a' }, { ...unv, actions: ['delete'], paths: [] },
  { ...unv, actions: [5, 'x_y_z'] }, { ...unv, actions: [{}] }, { ...unv, actions: [[]] }, { ...unv, actions: [['a_b', 'c']] },
  { ...unv, actions: [{ toString: 1 }] }, { ...unv, actions: ['', 'git_push'] }, { ...unv, actions: [null, true] },
  { ...unv, actions: ['none', 'read_repository'] }, { ...unv, actions: 'edit_file', action: 'delete', paths: ['p'] },
  { ...unv, action: U, actions: [] }, { ...unv, action: 7, actions: [] }, { ...unv, action: 0, actions: [] },
  { unverified: true }, { unverified: true, actions: ['edit_file'], paths: ['a'] },
];
for (const classified of UNVERIFIED) {
  for (const combo of COMBOS.filter((_, j) => j % 3 === 1)) add('decide', [{ classified, ...combo }]);
  add('decide', [{ classified, capabilities: [5, 'execute_command', 'external_account'], inWorkspace: false }]);
  add('decide', [{ classified, capabilities: [{}], inWorkspace: false }]);
  add('decide', [{ classified, capabilities: { length: 1 } }]);
}
for (const input of [U, null, 5, 'str', [], {}, { classified: U }, { capabilities: ['x'] }]) {
  add('decide', [input]);
  add('decideInput', [input]);
}
// Injected refusals: the floor on the host, then the person.
const FLOOR = [
  { classified: { ...base, actions: ['edit_file'], paths: ['a'] }, inWorkspace: false },
  { classified: { ...base, actions: ['git_push'] }, capabilities: ['execute_command'] },
  { classified: { ...base, actions: ['git_push'] }, capabilities: 'git_push' },
  { classified: base, capabilities: ['execute_command'] },
  { classified: { ...base, approval: 'capability', command: 'curl https://ok.test/', actions: ['network'] }, domains: ['ok.test'] },
  { classified: { ...base, approval: 'capability', command: 'curl https://ok.test/', actions: ['network', 'delete'], paths: ['a'] }, inWorkspace: false },
  { classified: { ...base, approval: 'capability', command: 'curl https://ok.test/', actions: ['network'] }, capabilities: ['edit_file'] },
];
for (const input of FLOOR) {
  for (const r of ['input', 'too_large', 'ambiguous', 'reply', 'trap', '', 'x'.repeat(50)]) add('decide', [input], [r]);
  add('decide', [input], ['ambiguous', null]);
  add('decide', [input], ['ambiguous', 'trap']);
}

// --- pickOption -------------------------------------------------------------------------------
const full = [{ optionId: 'a', kind: 'allow_once' }, { optionId: 'b', kind: 'allow_always' }, { optionId: 'c', kind: 'reject_once' },
  { optionId: 'd', kind: 'reject_always' }];
const OPTIONS = [
  full, ...full.map((o) => full.filter((x) => x !== o)), full.slice(0, 2), full.slice(2), [], [full[0]], [full[2]],
  [null, { optionId: 'a', kind: 'allow_once' }], [{ kind: 'allow_once' }, { optionId: 'z', kind: 'allow_once' }],
  [{ optionId: 5, kind: 'allow_once' }, { optionId: 'q', kind: 5 }], [{ optionId: 'allow_once' }, { optionId: 'reject_always', kind: null }],
  [{ optionId: 'x', kind: 'allow_once', name: 'Allow', extra: [{}] }, { optionId: 'x2', kind: 'allow_once' }],
  [0, '', 'allow_once', 5, [], { optionId: 'r', kind: 'reject_once' }], [{ optionId: '\ud83d', kind: 'reject_once' }],
  U, null, 'allow_once', { optionId: 'a', kind: 'allow_once' }, 5,
];
const WANTED = ['allow_once', 'allow_always', 'reject_once', 'reject_always', 'allow', 'allowance', 'reject', '', 'ALLOW_ONCE',
  'reject_never', U, null, 5, ['allow_once']];
for (const options of OPTIONS) {
  for (const wanted of WANTED) add('pickOption', [options, wanted]);
  add('pickInput', [options]);
}
for (const r of ['input', 'reply', 'trap', '']) add('pickOption', [full, 'allow_once'], [r]);
add('pickOption', [F({ optionId: R('o', 1000), kind: 'allow_once' }, 2200), 'reject_once']);

// --- write ------------------------------------------------------------------------------------
const exportsKeys = Object.keys(ca);
const fixture = {
  version: 1,
  source: {
    'code-actions.cjs': sha(modPath),
    'dav-parse.lock': sha(lockPath),
    NOEVIA_RS_REF: ref,
    DAV_PARSE_WASM_SHA256: wasmSha,
    node: process.version,
  },
  constants: {
    exports: JSON.stringify(exportsKeys),
    ACTIONS: JSON.stringify(ca.ACTIONS),
    COMMAND_KEYS: JSON.stringify(ca.COMMAND_KEYS),
    UNVERIFIED_ASK: JSON.stringify(ca.UNVERIFIED_ASK),
  },
  cases,
};
const out = path.join(__dirname, '..', 'tests', 'fixtures', 'host-node.v1.json');
fs.writeFileSync(out, `${JSON.stringify(fixture, null, 0).replace(/\},\{"fn"/g, '},\n{"fn"')}\n`);
const counts = {};
for (const c of cases) counts[c.fn] = (counts[c.fn] || 0) + 1;
console.log(`wrote ${out}: ${cases.length} cases ${JSON.stringify(counts)}, ${fs.statSync(out).size} bytes`);
