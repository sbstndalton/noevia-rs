#!/usr/bin/env node
'use strict';
// Records what each of noevia-core's ownership refusal stubs answers (council p2, pool item 8: Node shims and route
// tables). The stubs are the `refuseOwned`-style checks at the top of core server/index.cjs handleRequestInner: each
// answers a fixed refusal (503 or 405) for a request the Rust front owns, so a stale route table or a direct loopback
// call can never make two processes answer. They are the oracle here: the Rust side (crates/route-pins) proves the
// front dispatches every recorded (method, path) natively, which makes the stub unreachable through the front.
//
// Usage (on the builder, never on MS02): node tools/gen-node-refusal-stubs.cjs <core checkout> <noevia checkout> > \
//   crates/route-pins/fixtures/node-refusal-stubs.json
// The noevia checkout gives each stub's pairing references (build/web-supervisor.sh and deploy/tools/*.sh read the
// stubs' markers and route sets), which a deletion has to change in the same step.
// The probe set is every routes.toml path (parameters filled with fixed QA values) under all seven methods, plus
// trailing-slash spellings; each stub is asked with its switches unset and again with each switch it reads set.
const fs = require('node:fs');
const path = require('node:path');

const core = path.resolve(process.argv[2] || '.');
const server = path.join(core, 'server');
const noevia = process.argv[3] ? path.resolve(process.argv[3]) : null;
const routesToml = path.join(__dirname, '..', 'contracts', 'http', 'routes.toml');
const METHODS = ['GET', 'HEAD', 'POST', 'PUT', 'PATCH', 'DELETE', 'OPTIONS'];

const http = require(path.join(server, 'http.cjs'));

// The refusal calls in index.cjs, in their order: `require('./X').fn(req, res, {...})`.
const index = fs.readFileSync(path.join(server, 'index.cjs'), 'utf8');
// Two shapes: `(req, res, { parseRequestUrl, json })` before the request chain, and `(req, res, p, json)` (the parsed
// pathname) inside handleRequestScoped.
const handler = index.slice(index.indexOf('async function handleRequestInner'));
const callRe = /require\('\.\/([\w./-]+\.cjs)'\)\.(refuse\w*)\(req, res, (p, json|\{)/g;
const calls = [];
for (let m; (m = callRe.exec(handler));) {
  if (!calls.some((c) => c.file === m[1] && c.fn === m[2])) calls.push({ file: m[1], fn: m[2], pathname: m[3] === 'p, json' });
}

// Probe paths from routes.toml.
const rows = [];
for (const block of fs.readFileSync(routesToml, 'utf8').split('[[route]]').slice(1)) {
  const p = /^path = "([^"]+)"/m.exec(block);
  if (p && !/^catch_all = true/m.test(block)) rows.push(p[1]);
}
const fill = (pattern) => pattern
  .replace(/\{\*\?\}/g, '')
  .replace(/\{\*\}/g, 'qa-seg/qa-rest')
  .replace(/\{[A-Za-z]+\}/g, 'qa-1');
// probe path -> the routes.toml pattern it was made from (the first one, in file order).
const patternOf = new Map();
for (const r of rows) {
  const p = fill(r);
  if (!patternOf.has(p)) patternOf.set(p, r);
  if (!p.endsWith('/') && !patternOf.has(`${p}/`)) patternOf.set(`${p}/`, r);
}
// The non-/api pages the public-pages stub names.
for (const p of ['/about', '/privacy', '/device', '/.well-known/webauthn', '/']) if (!patternOf.has(p)) patternOf.set(p, null);
const probes = new Set(patternOf.keys());

function fakeRes() {
  const out = { status: null, headers: {}, body: '' };
  return {
    out,
    setHeader(k, v) { out.headers[k.toLowerCase()] = v; },
    getHeader(k) { return out.headers[k.toLowerCase()]; },
    removeHeader(k) { delete out.headers[k.toLowerCase()]; },
    writeHead(code, headers) {
      out.status = code;
      for (const [k, v] of Object.entries(headers || {})) out.headers[k.toLowerCase()] = v;
      return this;
    },
    write(chunk) { out.body += String(chunk); return true; },
    end(chunk) { if (chunk !== undefined) out.body += String(chunk); out.ended = true; },
  };
}

function envNames(file) {
  const src = fs.readFileSync(path.join(server, file), 'utf8');
  return [...new Set([...src.matchAll(/process\.env\.([A-Z][A-Z0-9_]+)|env\.([A-Z][A-Z0-9_]+)|'((?:NOEVIA|COWORK)_[A-Z0-9_]+)'/g)]
    .map((m) => m[1] || m[2] || m[3]).filter((n) => /^(NOEVIA|COWORK)_/.test(n)))].sort();
}

function ask(mod, fn, pathname, method, url) {
  const req = { method, url, headers: { host: 'localhost' }, socket: { remoteAddress: '127.0.0.1' } };
  const res = fakeRes();
  let refused;
  try {
    refused = pathname
      ? mod[fn](req, res, http.parseRequestUrl(req).pathname, http.json)
      : mod[fn](req, res, { parseRequestUrl: http.parseRequestUrl, badRequestUrl: http.badRequestUrl, json: http.json });
  } catch (err) {
    return { error: String(err && err.message) };
  }
  if (refused && typeof refused.then === 'function') return { error: 'async refusal' };
  if (!refused) return null;
  const bodyText = res.out.body;
  let body = bodyText;
  try { body = JSON.parse(bodyText); } catch { /* keep the text */ }
  const headers = {};
  for (const k of Object.keys(res.out.headers).sort()) headers[k] = String(res.out.headers[k]);
  return { status: res.out.status, headers, body_text: bodyText, body };
}

function record(file, fn, pathname, env) {
  const saved = {};
  for (const [k, v] of Object.entries(env)) { saved[k] = process.env[k]; process.env[k] = v; }
  const modPath = path.join(server, file);
  delete require.cache[require.resolve(modPath)];
  let mod;
  try { mod = require(modPath); } catch (err) { return { error: `require: ${err.message}` }; }
  const refusals = [];
  for (const p of [...probes].sort()) {
    for (const method of METHODS) {
      const r = ask(mod, fn, pathname, method, p);
      if (r) refusals.push({ method, path: p, pattern: patternOf.get(p), ...r });
    }
  }
  for (const [k, v] of Object.entries(saved)) { if (v === undefined) delete process.env[k]; else process.env[k] = v; }
  return { refusals };
}

// Values a switch is asked with: on-spellings the stubs use (`1`, `rust`, `native`).
const ON = ['1', 'rust', 'native'];
// What index.cjs takes from a stub besides its refusal function: `require('./x.cjs').name` and destructured or bound
// requires. A stub index.cjs uses for anything else can lose only its refusal function.
const escapeRe = (t) => t.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
function indexUses(file) {
  const uses = new Set();
  const q = escapeRe(file);
  for (const m of index.matchAll(new RegExp(`require\\('\\./${q}'\\)(?:\\.(\\w+))?`, 'g'))) uses.add(m[1] || '<module>');
  for (const m of index.matchAll(new RegExp(`(?:const|let|var)\\s+(\\{[^}]*\\}|\\w+)\\s*=\\s*require\\('\\./${q}'\\)\\s*[;\\n]`, 'g'))) {
    for (const n of m[1].replace(/[{}]/g, '').split(',').map((x) => x.trim().split(':')[0].trim()).filter(Boolean)) uses.add(n);
  }
  uses.delete('<module>');
  return [...uses].sort();
}
const lines = (f) => fs.readFileSync(path.join(server, f), 'utf8').split('\n').length;
const stubs = [];
for (const { file, fn, pathname } of calls) {
  const env = envNames(file);
  const base = record(file, fn, pathname, {});
  const switched = [];
  for (const name of env) {
    for (const value of ON) {
      const r = record(file, fn, pathname, { [name]: value });
      if (JSON.stringify(r.refusals) !== JSON.stringify(base.refusals)) switched.push({ env: { [name]: value }, ...r });
    }
  }
  const { execFileSync } = require('node:child_process');
  let importers = [];
  try {
    importers = execFileSync('grep', ['-rlF', `./${path.basename(file)}`, server, '--include=*.cjs'], { encoding: 'utf8' })
      .split('\n').filter(Boolean).map((f) => path.relative(core, f)).sort();
  } catch { /* none */ }
  const pairing = [];
  if (noevia) {
    for (const f of ['build/web-supervisor.sh', 'deploy/tools/assemble-release.sh']) {
      const at = path.join(noevia, f);
      if (!fs.existsSync(at)) continue;
      fs.readFileSync(at, 'utf8').split('\n').forEach((l, i) => { if (l.includes(path.basename(file))) pairing.push(`${f}:${i + 1}`); });
    }
  }
  // Every core file that names the stub by its file name (manifests, allowlists, preload lists, workflows, docs),
  // beyond the `./x.cjs` requires above.
  let mentions = [];
  try {
    mentions = execFileSync('grep', ['-rlF', path.basename(file), core, '--exclude-dir=node_modules', '--exclude-dir=.git'], { encoding: 'utf8' })
      .split('\n').filter(Boolean).map((f) => path.relative(core, f)).filter((f) => f !== `server/${file}` && !importers.includes(f)).sort();
  } catch { /* none */ }
  // A quoted name (a path string, a list entry, a JSON key) is a reference something reads; a bare name in a comment
  // or prose is only a mention.
  const quoted = new RegExp(`[\`'"][\\w./-]*${escapeRe(path.basename(file))}[\`'"]`);
  const stringRefs = mentions.filter((f) => quoted.test(fs.readFileSync(path.join(core, f), 'utf8')));
  mentions = mentions.filter((f) => !stringRefs.includes(f));
  stubs.push({ file: `server/${file}`, fn, pathname_arg: pathname, string_refs: stringRefs, mentions, lines: lines(file), env, importers, index_uses: indexUses(file), pairing, ...base, switched });
}

process.stdout.write(`${JSON.stringify({
  generator: 'tools/gen-node-refusal-stubs.cjs',
  core: process.env.CORE_REF || 'unknown',
  noevia: process.env.NOEVIA_REF || 'unknown',
  methods: METHODS,
  probes: probes.size,
  stubs,
}, null, 1)}\n`);
