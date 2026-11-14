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
//
// Council p5 (sbstndalton/noevia#2713, #2700): every parameter is filled twice (`qa-1` and a UUID, since some stubs
// refuse only UUID-shaped ids); each stub's exported OWNED_ROUTES is recorded so the Rust side can find owned routes
// without a recorded refusal; the shell's NOEVIA_*/COWORK_* switches are cleared first and every switch run restores
// the environment; pairing references cover all of noevia's build/ and deploy/ scripts and the stub's marker names;
// `behind` lists the core lines (outside the stubs) that still name each refused route, i.e. a Node handler a
// deletion would expose to a direct request; and the routes.toml the probes came from is hashed. CORE_REF and
// NOEVIA_REF are required.
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

if (!process.env.CORE_REF || (process.argv[3] && !process.env.NOEVIA_REF)) {
  process.stderr.write('gen-node-refusal-stubs: set CORE_REF (and NOEVIA_REF with a noevia checkout) to the recorded commits\n');
  process.exit(2);
}
const CORE_REF = process.env.CORE_REF;
const NOEVIA_REF = process.env.NOEVIA_REF || 'unknown';
// A switch inherited from the shell would change every base recording (the refs above are read first).
for (const k of Object.keys(process.env)) if (/^(NOEVIA|COWORK)_/.test(k)) delete process.env[k];

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
// The two ids (bins/noevia-server owned_routes.rs FILLS): a plain one and a UUID.
const FILLS = ['qa-1', '00000000-0000-4000-8000-000000000001'];
// Council p7: `{*}` is one segment in the front's matcher (routes.rs pattern_matches), and the project original's
// `{*}` is `uploads` (owned_routes.rs wildcard_fill); the old two-segment filling matched no `{*}` row.
const wildcardFill = (pattern) => (pattern === '/api/projects/{id}/{*}/original' ? 'uploads' : 'qa-seg');
const fill = (pattern, id = FILLS[0]) => pattern
  .replace(/\{\*\?\}/g, '')
  .replace(/\{\*\}/g, wildcardFill(pattern))
  .replace(/\{[A-Za-z]+\}/g, id);
// probe path -> the routes.toml pattern it was made from (the first one, in file order).
const patternOf = new Map();
for (const id of FILLS) {
  for (const r of rows) {
    const p = fill(r, id);
    if (!patternOf.has(p)) patternOf.set(p, r);
    if (!p.endsWith('/') && !patternOf.has(`${p}/`)) patternOf.set(`${p}/`, r);
  }
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

// The exported OWNED_ROUTES set as strings (`ALL /p` or `METHOD /p`), or null when the stub exports none.
function ownedRoutes(mod) {
  const set = mod && mod.OWNED_ROUTES;
  if (!set || typeof set[Symbol.iterator] !== 'function' || typeof set === 'string') return null;
  // Some stubs keep [regex, methods] pairs instead (rust-auth.cjs): not in routes.toml's spelling, not compared.
  const entries = [...set];
  if (entries.some((e) => typeof e !== 'string')) return null;
  return entries.sort();
}

function record(file, fn, pathname, env) {
  const saved = {};
  for (const [k, v] of Object.entries(env)) { saved[k] = process.env[k]; process.env[k] = v; }
  try {
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
    return { refusals, owned_routes: ownedRoutes(mod), exported: exportedStrings(mod) };
  } finally {
    for (const [k, v] of Object.entries(saved)) { if (v === undefined) delete process.env[k]; else process.env[k] = v; }
  }
}

// The marker names noevia's scripts grep for: exported `rust-…` string values (`rust-residual-fallbacks-v1`) and
// exported `RUST_…` constant names (`RUST_RESIDUAL_FALLBACKS_OWNED`).
function exportedStrings(mod) {
  const out = new Set();
  for (const [k, v] of Object.entries(mod || {})) {
    if (/^RUST_[A-Z0-9_]{4,}$/.test(k)) out.add(k);
    if (typeof v === 'string' && /^rust-[a-z0-9-]{3,}$/.test(v)) out.add(v);
  }
  return [...out].sort();
}

// Every text file under noevia's build/ and deploy/ and its workflows: the scripts that pair core with the front.
function pairingFiles(root) {
  const out = [];
  const walk = (d) => {
    let entries = [];
    try { entries = fs.readdirSync(d, { withFileTypes: true }); } catch { return; }
    for (const e of entries) {
      const at = path.join(d, e.name);
      if (e.isDirectory()) { if (!['node_modules', '.git', 'fixtures'].includes(e.name)) walk(at); }
      else if (/\.(sh|py|cjs|mjs|js|ya?ml|toml|txt)$|^Dockerfile/.test(e.name)) out.push(at);
    }
  };
  for (const d of ['build', 'deploy', '.github/workflows']) walk(path.join(root, d));
  return out.sort();
}
const pairingText = new Map();
if (noevia) for (const f of pairingFiles(noevia)) pairingText.set(path.relative(noevia, f), fs.readFileSync(f, 'utf8').split('\n'));

// The names noevia's scripts can know a stub by: its file name and its exported marker names. A marker value's
// version suffix is dropped (`rust-x-v1` -> `rust-x-v`) so a script reading another version still counts.
function pairingNeedles(file, exported) {
  return [...new Set([path.basename(file), ...exported.map((m) => m.replace(/-v\d+$/, '-v'))])];
}

// Core lines outside the refusal stubs that still name a refused route: a handler (or matcher) a direct request would
// reach once the stub is gone. Regex literals are read with `\/` as `/`; a parameter matches anything on the line
// between the literal pieces. Over-reporting is the safe direction: such a stub is "delete with handler", not "delete".
// Core files other than the stubs themselves (index.cjs counts, minus its stub calls; rust-*-bridge.cjs files count).
const stubFiles = new Set(calls.map((c) => path.join(server, c.file)));
const handlerFiles = [];
{
  const walk = (d) => {
    for (const e of fs.readdirSync(d, { withFileTypes: true })) {
      const at = path.join(d, e.name);
      if (e.isDirectory()) { if (!['node_modules', 'fixtures'].includes(e.name)) walk(at); }
      else if (e.name.endsWith('.cjs') && !e.name.endsWith('.test.cjs') && !stubFiles.has(at)) handlerFiles.push(at);
    }
  };
  walk(server);
}
const handlerText = handlerFiles.map((f) => [path.relative(core, f), fs.readFileSync(f, 'utf8').split('\n').map((l) => l.replace(/\\\//g, '/'))]);
function behind(patterns) {
  const out = new Map();
  for (const pattern of patterns) {
    const pieces = pattern.split(/\{[^}]*\}/).filter((x) => x && x !== '/');
    if (!pieces.length) continue;
    // A whole literal path must not continue as a longer literal (`/api/x` is not `/api/xy`).
    const exact = pieces.length === 1 && !pattern.includes('{');
    const hits = [];
    for (const [f, ls] of handlerText) {
      ls.forEach((l, i) => {
        const t = l.trim();
        if (t.startsWith('//') || t.startsWith('*') || /require\('\.\/rust-/.test(l)) return;
        let from = 0;
        for (const piece of pieces) {
          const at = l.indexOf(piece, from);
          if (at < 0) return;
          if (exact && /[A-Za-z0-9_-]/.test(l[at + piece.length] || '')) return;
          from = at + piece.length;
        }
        hits.push(`${f}:${i + 1}`);
      });
    }
    if (hits.length) out.set(pattern, hits);
  }
  return Object.fromEntries([...out].sort());
}
// Council p7 (CONTINUE 3 of p5): the methods a `behind` line itself compares the request method with
// (`req.method === 'PUT'`, `method == "GET"`, `['GET', 'HEAD'].includes(req.method)`). Only the hit line: a
// neighbour's test could belong to another route, and dropping a real handler is the unsafe direction. A negation
// (`!==`, `!=`) or no comparison records nothing, and the Rust side then counts the line for every method.
const textOf = new Map(handlerText);
function methodsNear(hit) {
  const at = hit.lastIndexOf(':');
  const ls = textOf.get(hit.slice(0, at));
  const i = Number(hit.slice(at + 1)) - 1;
  if (!ls) return null;
  const near = ls[i] || '';
  // Council p12: a negated list (`!['GET', 'HEAD'].includes(req.method)`, routes/models.cjs) tests the other methods.
  if (/method\s*!==?\s*['"]/.test(near) || /['"][A-Z]+['"]\s*!==?\s*(?:req\.)?method/.test(near)
    || /!\s*\[[^\]]*\]\.includes\((?:req\.)?method\)/.test(near)) return null;
  const named = new Set();
  for (const m of near.matchAll(/method\s*===?\s*['"]([A-Z]+)['"]/g)) named.add(m[1]);
  for (const m of near.matchAll(/['"]([A-Z]+)['"]\s*===?\s*(?:req\.)?method\b/g)) named.add(m[1]);
  for (const m of near.matchAll(/\[([^\]]*)\]\.includes\((?:req\.)?method\)/g)) {
    for (const q of m[1].matchAll(/['"]([A-Z]+)['"]/g)) named.add(q[1]);
  }
  const known = [...named].filter((x) => METHODS.includes(x)).sort();
  return known.length ? known : null;
}
function behindMethods(behindMap) {
  const out = {};
  for (const hit of [...new Set(Object.values(behindMap).flat())].sort()) {
    const m = methodsNear(hit);
    if (m) out[hit] = m;
  }
  return out;
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
  if (uses.size > 1) uses.delete('<module>');
  return [...uses].sort();
}
const lines = (f) => fs.readFileSync(path.join(server, f), 'utf8').split('\n').length;
// sbstndalton/noevia#2713 item 6: a bare use of the module (`const x = require('./rust-x.cjs')` then `x(...)`) is a
// use too; keep `<module>` when no name was captured.
const stubs = [];
for (const { file, fn, pathname } of calls) {
  const env = envNames(file);
  const base = record(file, fn, pathname, {});
  const switched = [];
  for (const name of env) {
    for (const value of ON) {
      const r = record(file, fn, pathname, { [name]: value });
      if (JSON.stringify(r.refusals) !== JSON.stringify(base.refusals)) switched.push({ env: { [name]: value }, refusals: r.refusals || [], ...(r.error ? { error: r.error } : {}) });
    }
  }
  let importers = [];
  try {
    importers = execFileSync('grep', ['-rlF', `./${path.basename(file)}`, server, '--include=*.cjs'], { encoding: 'utf8' })
      .split('\n').filter(Boolean).map((f) => path.relative(core, f)).sort();
  } catch { /* none */ }
  const pairing = [];
  const needles = pairingNeedles(file, base.exported || []);
  for (const [f, ls] of pairingText) {
    ls.forEach((l, i) => { if (needles.some((n) => l.includes(n))) pairing.push(`${f}:${i + 1}`); });
  }
  const refusedPatterns = [...new Set((base.refusals || []).map((r) => r.pattern).filter(Boolean))];
  const behindOf = behind(refusedPatterns);
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
  const { exported, ...kept } = base;
  stubs.push({ file: `server/${file}`, fn, pathname_arg: pathname, string_refs: stringRefs, mentions, lines: lines(file), env, importers, index_uses: indexUses(file), pairing, behind: behindOf, behind_methods: behindMethods(behindOf), ...kept, switched });
}

process.stdout.write(`${JSON.stringify({
  generator: 'tools/gen-node-refusal-stubs.cjs',
  core: CORE_REF,
  noevia: NOEVIA_REF,
  routes_toml_sha256: crypto.createHash('sha256').update(fs.readFileSync(routesToml)).digest('hex'),
  fills: FILLS,
  methods: METHODS,
  probes: probes.size,
  stubs,
}, null, 1)}\n`);
