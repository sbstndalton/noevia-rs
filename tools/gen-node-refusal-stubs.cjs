#!/usr/bin/env node
'use strict';
// Records what each of noevia-core's ownership refusal stubs answers (council p2, pool item 8: Node shims and route
// tables). The stubs are the `refuseOwned`-style checks at the top of core server/index.cjs handleRequestInner: each
// answers a fixed refusal (503 or 405) for a request the Rust front owns, so a stale route table or a direct loopback
// call can never make two processes answer. They are the oracle here: the Rust side (crates/route-pins) proves the
// front dispatches every recorded (method, path) natively, which makes the stub unreachable through the front.
//
// Usage (on the builder, never on MS02): node tools/gen-node-refusal-stubs.cjs <core checkout> > \
//   crates/route-pins/fixtures/node-refusal-stubs.json
// The probe set is every routes.toml path (parameters filled with fixed QA values) under all seven methods, plus
// trailing-slash spellings; each stub is asked with its switches unset and again with each switch it reads set.
const fs = require('node:fs');
const path = require('node:path');

const core = path.resolve(process.argv[2] || '.');
const server = path.join(core, 'server');
const routesToml = path.join(__dirname, '..', 'contracts', 'http', 'routes.toml');
const METHODS = ['GET', 'HEAD', 'POST', 'PUT', 'PATCH', 'DELETE', 'OPTIONS'];

const http = require(path.join(server, 'http.cjs'));

// The refusal calls in index.cjs, in their order: `require('./X').fn(req, res, {...})`.
const index = fs.readFileSync(path.join(server, 'index.cjs'), 'utf8');
const handler = index.slice(index.indexOf('async function handleRequestInner'));
const callRe = /require\('\.\/([\w./-]+\.cjs)'\)\.(refuse\w*)\(req, res,/g;
const calls = [];
for (let m; (m = callRe.exec(handler));) {
  if (!calls.some((c) => c.file === m[1] && c.fn === m[2])) calls.push({ file: m[1], fn: m[2] });
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
const probes = new Set();
for (const r of rows) {
  const p = fill(r);
  probes.add(p);
  if (!p.endsWith('/')) probes.add(`${p}/`);
}
// The non-/api pages the public-pages stub names.
for (const p of ['/about', '/privacy', '/device', '/.well-known/webauthn', '/']) probes.add(p);

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
  return [...new Set([...src.matchAll(/process\.env\.([A-Z][A-Z0-9_]+)|env\.([A-Z][A-Z0-9_]+)|\['([A-Z][A-Z0-9_]+)'\]/g)]
    .map((m) => m[1] || m[2] || m[3]).filter((n) => /^(NOEVIA|COWORK)_/.test(n)))].sort();
}

function ask(mod, fn, method, url) {
  const req = { method, url, headers: { host: 'localhost' }, socket: { remoteAddress: '127.0.0.1' } };
  const res = fakeRes();
  let refused;
  try {
    refused = mod[fn](req, res, { parseRequestUrl: http.parseRequestUrl, badRequestUrl: http.badRequestUrl, json: http.json });
  } catch (err) {
    return { error: String(err && err.message) };
  }
  if (refused && typeof refused.then === 'function') return { error: 'async refusal' };
  if (!refused) return null;
  let body = res.out.body;
  try { body = JSON.parse(body); } catch { /* keep the text */ }
  const headers = {};
  for (const k of Object.keys(res.out.headers).sort()) headers[k] = String(res.out.headers[k]);
  return { status: res.out.status, headers, body };
}

function record(file, fn, env) {
  const saved = {};
  for (const [k, v] of Object.entries(env)) { saved[k] = process.env[k]; process.env[k] = v; }
  const modPath = path.join(server, file);
  delete require.cache[require.resolve(modPath)];
  let mod;
  try { mod = require(modPath); } catch (err) { return { error: `require: ${err.message}` }; }
  const refusals = [];
  for (const p of [...probes].sort()) {
    for (const method of METHODS) {
      const r = ask(mod, fn, method, p);
      if (r) refusals.push({ method, path: p, ...r });
    }
  }
  for (const [k, v] of Object.entries(saved)) { if (v === undefined) delete process.env[k]; else process.env[k] = v; }
  return { refusals };
}

// Values a switch is asked with: on-spellings the stubs use (`1`, `rust`, `native`).
const ON = ['1', 'rust', 'native'];
const lines = (f) => fs.readFileSync(path.join(server, f), 'utf8').split('\n').length;
const stubs = [];
for (const { file, fn } of calls) {
  const env = envNames(file);
  const base = record(file, fn, {});
  const switched = [];
  for (const name of env) {
    for (const value of ON) {
      const r = record(file, fn, { [name]: value });
      if (JSON.stringify(r.refusals) !== JSON.stringify(base.refusals)) switched.push({ env: { [name]: value }, ...r });
    }
  }
  const { execFileSync } = require('node:child_process');
  let importers = [];
  try {
    importers = execFileSync('grep', ['-rlF', `./${path.basename(file)}`, server, '--include=*.cjs'], { encoding: 'utf8' })
      .split('\n').filter(Boolean).map((f) => path.relative(core, f)).sort();
  } catch { /* none */ }
  stubs.push({ file: `server/${file}`, fn, lines: lines(file), env, importers, ...base, switched });
}

process.stdout.write(`${JSON.stringify({
  generator: 'tools/gen-node-refusal-stubs.cjs',
  core: process.env.CORE_REF || 'unknown',
  methods: METHODS,
  probes: probes.size,
  stubs,
}, null, 1)}\n`);
