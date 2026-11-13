#!/usr/bin/env node
// Test-oracle glue only: asks noevia-core's own server/code-harness-config.cjs, on synthetic inputs,
// what it pins and what it refuses, and what writeHarnessConfig leaves on disk in fresh temp dirs.
//   node gen-fixtures.cjs <noevia-core>/server [output.json]
// Output defaults to ../tests/fixtures/code-harness-config.v1.json next to this script.
'use strict';
const fs = require('node:fs'), os = require('node:os'), path = require('node:path'), crypto = require('node:crypto');
const Module = require('node:module');

const serverDir = path.resolve(process.argv[2] || '');
const outFile = path.resolve(process.argv[3] || path.join(__dirname, '..', 'tests', 'fixtures', 'code-harness-config.v1.json'));
const file = path.join(serverDir, 'code-harness-config.cjs');
const source = fs.readFileSync(file, 'utf8');

// The module as published, plus its private helpers (writePinned, dshPatch) reached through the same
// compiled source: nothing in it is changed, two names are only exported in addition.
const mod = new Module(file, module);
mod.filename = file;
mod.paths = Module._nodeModulePaths(serverDir);
mod._compile(`${source}\n;module.exports.__writePinned = writePinned; module.exports.__dshPatch = dshPatch;`, file);
const hc = mod.exports;

// Fixture numbers: JSON cannot hold these, so they are spelled out.
const SPECIAL = { NaN: NaN, Infinity: Infinity, '-Infinity': -Infinity, '-0': -0 };
const num = (v) => (typeof v === 'object' && v !== null ? SPECIAL[v.num] : v);

const contents = [];
const contentIndex = new Map();
const ref = (text) => {
  if (!contentIndex.has(text)) { contentIndex.set(text, contents.length); contents.push(text); }
  return contentIndex.get(text);
};
const errOf = (e) => (e && typeof e.status === 'number'
  ? { status: e.status, message: e.message }
  : e && e.code ? { code: e.code, syscall: e.syscall || null } : { status: null, message: String(e && e.message) });

const HARNESSES = ['opencode', 'claude-code', 'codex', 'qwen-code', 'pi', 'deepseek', ' pi ', '\tdeepseek\n',
  ' opencode﻿', '\u0085pi', '　claude-code ', 'PI', 'Opencode', 'aider', 'goose', '', ' ', null,
  'opencode.json', 'unicodé\u{1F600}'];
const MODELS = ['Ornith-1.5-9B-Q5_K_M', '', '   ', null, 'mödel/“quoted” \\ "x" \n tab\t end', '\u{1F600} emoji',
  '__proto__', '123', 'a b c', ' 　m　 ', '\u0085m', '\u0001ctl\u007f\u009f', 'local/x:7b', '﻿bom﻿'];
const ENGINES = ['http://llama:8080/v1', 'http://llama:8080', 'http://llama:8080/', 'https://e.example/v1///', 'http://', 'https://',
  'http:///', 'HTTP://x/v1', 'ftp://x', 'llama:8080', '/v1', '', null, ' http://x/v1 ', 'http://x/v1/v1', 'http://x/V1',
  'http://x/v1x', 'http://x//v1', ' https://x﻿', 'https://hé.example/p?q=1#f'];
const KEYS = [null, '', 'k', 'k"e\\y\n \u{1F600}'];
const TOKENS = [[undefined, undefined], [8192, undefined], [undefined, 1024], [0, 0], [1000.5, 99.99], [-5, -1],
  [2147483653, 4294967297], [1e21, 1.5e-7], [{ num: 'NaN' }, { num: 'Infinity' }], [{ num: '-0' }, { num: '-Infinity' }],
  [131072, 32768], [2 ** 53 + 2, 0.1 + 0.2]];

const inputs = [];
for (const harness of HARNESSES) for (const model of MODELS) inputs.push({ harness, model, engine: 'http://llama:8080/v1' });
for (const harness of HARNESSES) for (const engine of ENGINES) inputs.push({ harness, model: 'm', engine });
for (const harness of HARNESSES) for (const apiKey of KEYS) for (const [c, o] of TOKENS) {
  const i = { harness, model: 'Ornith-1.5-9B-Q5_K_M', engine: 'http://llama:8080/v1', apiKey };
  if (c !== undefined) i.contextTokens = c;
  if (o !== undefined) i.outputTokens = o;
  inputs.push(i);
}
const callArgs = (i) => {
  const a = { ...i };
  if ('contextTokens' in a) a.contextTokens = num(a.contextTokens);
  if ('outputTokens' in a) a.outputTokens = num(a.outputTokens);
  return a;
};

const out = {
  source: { file: 'server/code-harness-config.cjs', sha256: crypto.createHash('sha256').update(source).digest('hex') },
  node: process.version,
  constants: {
    PERMISSION: hc.PERMISSION, EDIT_TOOLS: hc.EDIT_TOOLS, PI_READ_TOOLS: hc.PI_READ_TOOLS,
    DSH_NO_ASK_TOOLS: hc.DSH_NO_ASK_TOOLS, DSH_DISABLED: hc.DSH_DISABLED,
    claudeSecuritySettings: JSON.stringify(hc.claudeSecuritySettings()),
  },
  contents,
};

out.configFor = inputs.map((i) => {
  try {
    const r = hc.configFor(callArgs(i));
    return { in: i, out: { name: r.name, json: JSON.stringify(r.json) } };
  } catch (e) { return { in: i, err: errOf(e) }; }
});

out.pinFilesFor = inputs.map((i) => {
  try {
    const r = hc.pinFilesFor(callArgs(i));
    return { in: i, out: { harness: r.harness, files: r.files.map((f) => ({ base: f.base, path: f.path, content: ref(f.content) })),
      permission: r.permission, model: r.model, endpoint: r.endpoint } };
  } catch (e) { return { in: i, err: errOf(e) }; }
});

out.cwdPinPaths = HARNESSES.map((harness) => ({ harness, out: hc.cwdPinPaths(harness) }));

const DSH = [];
for (const name of ['m', 'q"u\\o\nte', '\u{1F600}']) for (const baseURL of ['http://x/v1', 'https://h/a b/v1'])
  for (const apiKey of [null, '', 'k', 'a"b']) for (const [c, o] of TOKENS) DSH.push({ name, baseURL, apiKey, contextTokens: c ?? 32768, outputTokens: o ?? 4096 });
out.dshPatch = DSH.map((i) => ({ in: i, out: ref(hc.__dshPatch({ ...i, contextTokens: num(i.contextTokens), outputTokens: num(i.outputTokens) })) }));

// ── Writes ───────────────────────────────────────────────────────────────────────────────────
const BASE = { model: 'Ornith-1.5-9B-Q5_K_M', engine: 'http://llama:8080/v1', apiKey: 'k' };
const D = (p, mode = 0o755) => ({ op: 'dir', path: p, mode });
const F = (p, content, mode = 0o644) => ({ op: 'file', path: p, content, mode });
const L = (p, target) => ({ op: 'link', path: p, target });
const CH = (p, mode) => ({ op: 'chmod', path: p, mode });
const ROOTS = [D('cwd'), D('home'), D('outside'), F('outside/keep.txt', 'outside\n')];
const W = [
  { name: 'opencode fresh', setup: ROOTS, call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'opencode', ...BASE } } },
  { name: 'opencode no home', setup: ROOTS, call: { cwd: '<T>/cwd', home: null, options: { harness: 'opencode', ...BASE } } },
  { name: 'opencode no cwd', setup: ROOTS, call: { cwd: null, home: '<T>/home', options: { harness: 'opencode', ...BASE } } },
  { name: 'opencode empty cwd', setup: ROOTS, call: { cwd: '', home: '<T>/home', options: { harness: 'opencode', ...BASE } } },
  { name: 'opencode owner', setup: ROOTS, call: { cwd: '<T>/cwd', home: '<T>/home', owner: { uid: 1000, gid: 1001 }, options: { harness: 'opencode', ...BASE } } },
  { name: 'claude-code owner', setup: ROOTS, call: { cwd: '<T>/cwd', home: '<T>/home', owner: { uid: 4242, gid: 0 }, options: { harness: 'claude-code', ...BASE } } },
  { name: 'claude-code no home writes cwd first', setup: ROOTS, call: { cwd: '<T>/cwd', home: null, owner: { uid: 7, gid: 8 }, options: { harness: 'claude-code', ...BASE } } },
  { name: 'claude-code empty home', setup: ROOTS, call: { cwd: '<T>/cwd', home: '', options: { harness: 'claude-code', ...BASE } } },
  { name: 'qwen-code', setup: ROOTS, call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'qwen-code', ...BASE, contextTokens: 65536 } } },
  { name: 'pi owner', setup: ROOTS, call: { cwd: '<T>/cwd', home: '<T>/home', owner: { uid: 1, gid: 2 }, options: { harness: 'pi', ...BASE } } },
  { name: 'pi no home', setup: ROOTS, call: { cwd: '<T>/cwd', home: null, options: { harness: 'pi', ...BASE } } },
  { name: 'deepseek', setup: ROOTS, call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'deepseek', ...BASE, apiKey: null, outputTokens: 2048 } } },
  { name: 'unknown harness writes nothing', setup: ROOTS, call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'aider', ...BASE } } },
  { name: 'codex writes nothing', setup: ROOTS, call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'codex', ...BASE } } },
  { name: 'missing model writes nothing', setup: ROOTS, call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'pi', ...BASE, model: ' ' } } },
  { name: 'cwd .claude is a link', setup: [...ROOTS, L('cwd/.claude', '<T>/outside')], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'claude-code', ...BASE } } },
  { name: 'cwd .claude is a relative link', setup: [...ROOTS, L('cwd/.claude', '../outside')], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'claude-code', ...BASE } } },
  { name: 'cwd .claude is a dangling link', setup: [...ROOTS, L('cwd/.claude', '<T>/nowhere')], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'claude-code', ...BASE } } },
  { name: 'home .pi/agent is a link', setup: [...ROOTS, D('home/.pi', 0o700), L('home/.pi/agent', '<T>/outside')], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'pi', ...BASE } } },
  { name: 'home .pi/agent/extensions is a link', setup: [...ROOTS, D('home/.pi', 0o700), D('home/.pi/agent', 0o750), L('home/.pi/agent/extensions', '<T>/outside')], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'pi', ...BASE } } },
  { name: 'home .pi is a file', setup: [...ROOTS, F('home/.pi', 'not a dir\n')], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'pi', ...BASE } } },
  { name: 'home .dsh/profiles is a file', setup: [...ROOTS, D('home/.dsh'), F('home/.dsh/profiles', 'x')], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'deepseek', ...BASE } } },
  { name: 'opencode.json is a directory', setup: [...ROOTS, D('cwd/opencode.json')], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'opencode', ...BASE } } },
  { name: 'opencode.json is a link to a file', setup: [...ROOTS, L('cwd/opencode.json', '<T>/outside/keep.txt')], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'opencode', ...BASE } } },
  { name: 'opencode.json is a dangling link', setup: [...ROOTS, L('cwd/opencode.json', '<T>/outside/new.txt')], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'opencode', ...BASE } } },
  { name: 'opencode.json is a link to a directory', setup: [...ROOTS, L('cwd/opencode.json', '<T>/outside')], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'opencode', ...BASE } } },
  { name: 'home settings.json is a link after cwd written', setup: [...ROOTS, D('home/.claude', 0o755), L('home/.claude/settings.json', '<T>/outside/keep.txt')], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'claude-code', ...BASE } } },
  { name: 'existing file overwritten', setup: [...ROOTS, F('cwd/opencode.json', '{"permission":{"edit":"allow"}}\n', 0o666)], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'opencode', ...BASE } } },
  { name: 'existing dirs kept with their mode', setup: [...ROOTS, D('cwd/.qwen', 0o775), F('cwd/.qwen/other.json', 'keep', 0o640), D('home/.qwen', 0o711), F('home/.qwen/settings.json', 'old', 0o604)], call: { cwd: '<T>/cwd', home: '<T>/home', owner: { uid: 3, gid: 3 }, options: { harness: 'qwen-code', ...BASE } } },
  { name: 'cwd with trailing slash and dot segments', setup: [...ROOTS, D('cwd2')], call: { cwd: '<T>/cwd/./../cwd2//', home: '<T>//home/', options: { harness: 'claude-code', ...BASE } } },
  { name: 'cwd root itself is a link (followed)', setup: [...ROOTS, L('cwdlink', '<T>/outside')], call: { cwd: '<T>/cwdlink', home: '<T>/home', options: { harness: 'opencode', ...BASE } } },
  { name: 'home does not exist', setup: ROOTS, call: { cwd: '<T>/cwd', home: '<T>/missing', options: { harness: 'pi', ...BASE } } },
  { name: 'cwd does not exist (top-level file)', setup: ROOTS, call: { cwd: '<T>/missing', home: '<T>/home', options: { harness: 'opencode', ...BASE } } },
  { name: 'filesystem root', setup: ROOTS, call: { cwd: '/', home: '<T>/home', options: { harness: 'opencode', ...BASE } } },
  { name: 'read-only cwd', setup: [...ROOTS, CH('cwd', 0o500)], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'claude-code', ...BASE } } },
  { name: 'unsearchable home', setup: [...ROOTS, D('home/.pi', 0o700), CH('home/.pi', 0o600)], call: { cwd: '<T>/cwd', home: '<T>/home', options: { harness: 'pi', ...BASE } } },
  { name: 'chown fails on first call', setup: ROOTS, call: { cwd: '<T>/cwd', home: '<T>/home', owner: { uid: 5, gid: 5 }, options: { harness: 'claude-code', ...BASE } }, failChownAt: 1 },
  { name: 'chown fails on a file', setup: ROOTS, call: { cwd: '<T>/cwd', home: '<T>/home', owner: { uid: 5, gid: 5 }, options: { harness: 'claude-code', ...BASE } }, failChownAt: 2 },
  { name: 'owner uid zero', setup: ROOTS, call: { cwd: '<T>/cwd', home: '<T>/home', owner: { uid: 0, gid: 0 }, options: { harness: 'opencode', ...BASE } } },
];
// writePinned on its own, for relative paths noevia never passes but the function accepts.
const P = [
  ['a/b/c.txt'], ['../escape.txt'], ['a/../b.txt'], ['/abs/x.txt'], ['a//b.txt'], ['./a.txt'], ['a/./b.txt'], ['x/'], [''], ['.'],
  ['a/b/../../..'], ['a/b/../../../cwd/z.txt'], ['é\u{1F600}/f.txt'], ['link/inner.txt', [L('cwd/link', '<T>/outside')]],
  ['a/link//x', [D('cwd/a'), L('cwd/a/link', '<T>/outside')]], ['a//f', [D('cwd/a'), L('cwd/a/f', '<T>/outside/keep.txt')]],
];
for (const [relative, extra = []] of P) W.push({ name: `writePinned ${JSON.stringify(relative)}`, setup: [...ROOTS, ...extra], pinned: { root: '<T>/cwd', relative, content: 'pinned é\n' } });
W.push({ name: 'writePinned root slash', setup: ROOTS, pinned: { root: '/', relative: 'x', content: 'x' } });

const fill = (s, T) => (typeof s === 'string' ? s.split('<T>').join(T) : s);
const strip = (s, T) => (typeof s === 'string' ? s.split(T).join('<T>') : s);

function setup(T, ops) {
  const chmods = [];
  for (const o of ops) {
    const p = path.join(T, o.path);
    if (o.op === 'dir') { fs.mkdirSync(p); fs.chmodSync(p, o.mode); }
    else if (o.op === 'file') { fs.writeFileSync(p, o.content); fs.chmodSync(p, o.mode); }
    else if (o.op === 'link') fs.symlinkSync(fill(o.target, T), p);
    else if (o.op === 'chmod') chmods.push(o);
  }
  for (const o of chmods) fs.chmodSync(path.join(T, o.path), o.mode);
}
function tree(T) {
  const entries = [];
  const walk = (rel) => {
    const abs = rel ? path.join(T, rel) : T;
    let names;
    try { names = fs.readdirSync(abs); } catch (e) { entries.push({ path: rel, unreadable: e.code }); return; }
    for (const name of names.sort()) {
      const r = rel ? `${rel}/${name}` : name, p = path.join(T, r);
      const st = fs.lstatSync(p);
      if (st.isSymbolicLink()) entries.push({ path: r, type: 'link', target: strip(fs.readlinkSync(p), T) });
      else if (st.isDirectory()) { entries.push({ path: r, type: 'dir', mode: st.mode & 0o777 }); walk(r); }
      else entries.push({ path: r, type: 'file', mode: st.mode & 0o777, content: fs.readFileSync(p, 'utf8') });
    }
  };
  walk('');
  return entries;
}
function restore(T, ops) {
  for (const o of ops) if (o.op === 'chmod') fs.chmodSync(path.join(T, o.path), 0o700);
}

const realChown = fs.lchownSync;
out.umask = process.umask();
out.writes = W.map((w) => {
  const T = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-hconf-'));
  const chowns = [];
  fs.lchownSync = (p, uid, gid) => {
    chowns.push([strip(p, T), uid, gid]);
    if (w.failChownAt === chowns.length) throw Object.assign(Error(`EPERM: operation not permitted, lchown '${p}'`), { code: 'EPERM', syscall: 'lchown' });
  };
  let result;
  try {
    setup(T, w.setup);
    if (w.pinned) {
      hc.__writePinned(fill(w.pinned.root, T), w.pinned.relative, w.pinned.content, null);
      result = { ok: null };
    } else {
      const c = w.call;
      const r = hc.writeHarnessConfig({ cwd: fill(c.cwd, T), home: fill(c.home, T), owner: c.owner || null, ...callArgs(c.options) });
      result = { ok: r };
    }
  } catch (e) { result = { err: errOf(e) }; }
  finally { fs.lchownSync = realChown; }
  restore(T, w.setup);
  const entry = { ...w, result, chowns, tree: tree(T) };
  fs.rmSync(T, { recursive: true, force: true });
  return entry;
});

fs.writeFileSync(outFile, JSON.stringify(out, null, 1) + '\n');
process.stdout.write(`${outFile}: ${out.configFor.length} configFor, ${out.pinFilesFor.length} pinFilesFor, ${out.dshPatch.length} dshPatch, ${out.writes.length} writes, ${contents.length} distinct contents\n`);
