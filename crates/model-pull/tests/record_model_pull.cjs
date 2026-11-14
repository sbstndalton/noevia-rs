#!/usr/bin/env node
'use strict';
// Recorded model pull and MTP artifact checks for crates/model-pull (track R11): the contract src/ is replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/model-pull/tests/record_model_pull.cjs > crates/model-pull/tests/fixtures/node-model-pull.v1.json
//   ... node crates/model-pull/tests/record_model_pull.cjs --check crates/model-pull/tests/fixtures/node-model-pull.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Nothing from node_modules and no dav-parse.wasm. Every expectation comes from noevia-core's own code; the inputs are
// synthetic and QA-named (hand-made edges plus a seeded PRNG):
//   inspect  mtp-artifact.cjs inspect(buffer) over GGUF headers built here (valid, truncated, oversized, malformed).
//   names    models.cjs deriveUserModelName (cut out of the pinned file and run as it is).
//   quants   llamacpp-variants.cjs variants() over synthetic Hugging Face sibling lists (its fetchJson stubbed).
//   pull     POST /api/models/pull: routes/models.cjs's own lines (the write prelude, the pull block, and the routes
//            wrapper's invalidation, cut out of the pinned file and run as they are) with http.cjs readBody over the raw
//            body, over a real model-manager.cjs / llamacpp-manager.cjs manager (kinds none, lemonade, llamacpp) whose
//            fetchJson and fetchStream are stubs; a rejection is answered with http.cjs errorResponse as index.cjs does.
//            Recorded: the status, the body, the invalidations, the manager requests (URL, method, headers, body,
//            deadline), the router event stream opened, and the llama.cpp download tracker's state file afterwards.
//   mtp      GET /api/models/mtp-artifact: the same route lines and the real mtp-artifact.cjs (fresh per scenario, so
//            its cache and running checks are a scenario's) with the global fetch stubbed by real Response objects
//            (bodies streamed in chunks, failures as undici raises them) and a fake clock. A scenario is a list of
//            steps at given times; a step of several requests runs them at once (every fetch held until all reached
//            it). Recorded per request: the status, the body, the manager requests; per step: the fetches (sorted
//            for a step run at once).
// Each source file's sha256 is recorded, so a changed Node file needs a fresh recording. Node's console output goes to
// stderr; stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
const server = path.join(root, 'server');
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => process.stderr.write(`${a.map(String).join(' ')}\n`);

// ── The clock: Date.now() and new Date() read it ───────────────────────────────────────────
const RealDate = Date;
let clock = RealDate.UTC(2026, 10, 14, 12, 0, 0, 0);
globalThis.Date = class QaDate extends RealDate {
  constructor(...a) { if (a.length) super(...a); else super(clock); }
  static now() { return clock; }
};

const SOURCE_FILES = ['server/routes/models.cjs', 'server/models.cjs', 'server/mtp-artifact.cjs', 'server/http.cjs', 'server/model-manager.cjs',
  'server/llamacpp-manager.cjs', 'server/llamacpp-variants.cjs', 'server/llamacpp-downloads.cjs', 'server/inference-maintenance.cjs'];
const sources = Object.fromEntries(SOURCE_FILES.map((f) => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex')]));
const req = (f) => require(path.join(server, f));
const { readBody, errorResponse } = req('http.cjs');
const { createModelManager } = req('model-manager.cjs');
const { variants: llamacppVariants } = req('llamacpp-variants.cjs');
const MTP_FILE = path.join(server, 'mtp-artifact.cjs');
let mtp = require(MTP_FILE);
function freshMtp() { delete require.cache[require.resolve(MTP_FILE)]; mtp = require(MTP_FILE); }
const routeRequire = (m) => { const full = path.resolve(server, 'routes', m); return full === MTP_FILE ? mtp : require(full); };

// Anything a stub did not expect fails the recording.
const problems = [];
globalThis.fetch = async (url) => { problems.push(`unexpected fetch ${url}`); throw TypeError('fetch failed'); };

// ── The lines of routes/models.cjs and models.cjs that are run as they are ──────────────────
const blocks = {};
function cutter(file) {
  const text = fs.readFileSync(path.join(root, file), 'utf8');
  return (name, start, end) => {
    const from = text.indexOf(start);
    if (from < 0 || text.indexOf(start, from + 1) >= 0) throw new Error(`${file} holds ${JSON.stringify(start.slice(0, 60))} ${from < 0 ? 'nowhere' : 'twice'}`);
    const stop = text.indexOf(end, from);
    if (stop < 0) throw new Error(`${file}: no end after ${name}`);
    const to = stop + end.length;
    const block = text.slice(from, to);
    blocks[name] = { file, lines: [text.slice(0, from).split('\n').length, text.slice(0, to).split('\n').length - 1],
      sha256: crypto.createHash('sha256').update(block).digest('hex') };
    return block;
  };
}
const routes = cutter('server/routes/models.cjs');
const PRELUDE = routes('prelude', "    if (!['GET', 'HEAD'].includes(req.method || 'GET') && /^\\/api\\/(models|model-manager|auto-roles)(\\/|$)/.test(p)) {",
  "      return json(res, 403, { error: 'administrator required' });\n    }\n");
const MTP = routes('mtp', "    if (p === '/api/models/mtp-artifact' && req.method === 'GET') {", '\n    }\n');
const PULL = routes('pull', "    if (p === '/api/models/pull' && req.method === 'POST') {", '\n    }\n');
const RUN = routes('run', '    const run = async () => {', '\n    };\n');
const DERIVE = cutter('server/models.cjs')('derive', '  function deriveUserModelName(checkpoint) {', '\n  }\n');
const AsyncFunction = (async () => {}).constructor;
const route = new AsyncFunction('req', 'res', 'ctx', 'json', 'readBody', 'modelManager', 'service', 'require',
  `const PASS = Symbol('unhandled');\n${DERIVE}\n  async function handle(req, res, { path: p, authn, url }) {\n${PRELUDE}${MTP}${PULL}    return PASS;\n  }\n${RUN}    return run();`);
const deriveUserModelName = new Function(`${DERIVE}\nreturn deriveUserModelName;`)();

// ── Deterministic inputs ────────────────────────────────────────────────────────────────────
let seed = 0x2026_1114;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const pick = (list) => list[Math.floor(rand() * list.length)];
const int = (n) => Math.floor(rand() * n);
const viaJson = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));
const clean = (o) => Object.fromEntries(Object.entries(o).filter(([, v]) => v !== undefined));
const LIMIT = mtp.LIMIT;

// Bytes from parts: [{ b64 } | { fill: byte, n }]; the replay rebuilds them the same way.
function fromParts(parts) { return Buffer.concat(parts.map((p) => (p.b64 !== undefined ? Buffer.from(p.b64, 'base64') : Buffer.alloc(p.n, p.fill)))); }
// Parts for bytes: small ones as base64, long runs of one byte as fills.
function toParts(bytes) {
  const parts = [];
  let at = 0;
  while (at < bytes.length) {
    let run = 1;
    while (at + run < bytes.length && bytes[at + run] === bytes[at]) run += 1;
    if (run >= 64) { parts.push({ fill: bytes[at], n: run }); at += run; continue; }
    let end = at;
    while (end < bytes.length) {
      let r = 1;
      while (end + r < bytes.length && bytes[end + r] === bytes[end]) r += 1;
      if (r >= 64) break;
      end += r;
    }
    parts.push({ b64: bytes.subarray(at, end).toString('base64') });
    at = end;
  }
  return parts;
}

// ── GGUF builder ────────────────────────────────────────────────────────────────────────────
const u32 = (n) => { const b = Buffer.alloc(4); b.writeUInt32LE(n >>> 0); return b; };
const u64 = (n) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b; };
const gstr = (s) => { const b = Buffer.isBuffer(s) ? s : Buffer.from(s, 'utf8'); return Buffer.concat([u64(b.length), b]); };
function gval(type, v) {
  if (type === 8) return gstr(v);
  if (Buffer.isBuffer(v)) return v; // raw bytes for a value
  // Integers are written as their two's-complement bytes.
  switch (type) {
    case 0: case 1: case 7: return Buffer.from([Number(v) & 0xff]);
    case 2: case 3: { const b = Buffer.alloc(2); b.writeUInt16LE(Number(v) & 0xffff); return b; }
    case 4: case 5: return u32(Number(v));
    case 6: { const b = Buffer.alloc(4); b.writeFloatLE(Number(v)); return b; }
    case 8: return gstr(v);
    case 9: return Buffer.concat([u32(v.t), u64(v.n ?? v.items.length), ...v.items.map((x) => gval(v.t, x))]);
    case 10: case 11: return u64(BigInt.asUintN(64, BigInt(v)));
    case 12: { const b = Buffer.alloc(8); b.writeDoubleLE(Number(v)); return b; }
    default: return Buffer.alloc(0);
  }
}
function gguf({ magic = Buffer.from('GGUF'), version = 3, count, kvCount, kv = [], tensors = [] } = {}) {
  const parts = [magic, u32(version), u64(count ?? tensors.length), u64(kvCount ?? kv.length)];
  for (const e of kv) parts.push(gstr(e.key), u32(e.type), gval(e.type, e.value));
  for (const t of tensors) {
    const shape = t.shape ?? [4096, 2];
    parts.push(gstr(t.name), u32(t.dims ?? shape.length), ...shape.map(u64), u32(t.type ?? 1), u64(t.offset ?? 0));
  }
  return Buffer.concat(parts);
}
const ARCH = (a) => ({ key: 'general.architecture', type: 8, value: a });
const LAYERS = (n, type = 4) => ({ key: 'qa_arch.nextn_predict_layers', type, value: n });
const SPLIT = (n, type = 2) => ({ key: 'split.count', type, value: n });
const T = (name, extra = {}) => ({ name, ...extra });
const NAMES = ['blk.0.attn_q.weight', 'blk.12.nextn.eh_proj.weight', 'mtp.0.ffn.weight', 'blk.3.mtp_proj.weight', 'NEXTN_head', 'blk.xnextn.weight',
  'blk.nextnx.0', 'output.weight', 'token_embd.weight', 'blk.1.MTP.norm', 'mtp', 'blk.mtp', 'blk.é.nextn.weight', 'blk.😀.mtp_x', '.mtp.', 'a..nextn_'];

// ── inspect ─────────────────────────────────────────────────────────────────────────────────
const inspectCases = [];
function addInspect(bytes, note) {
  let out;
  try { out = { out: viaJson(mtp.inspect(bytes)) }; } catch (e) { out = { error: e.message }; }
  inspectCases.push({ ...(note ? { note } : {}), parts: toParts(bytes), bytes: bytes.length, ...out });
}
function inspectEdges() {
  const base = { kv: [ARCH('qa_arch'), LAYERS(1), SPLIT(1)], tensors: [T('blk.0.nextn.eh_proj.weight'), T('output.weight')] };
  const whole = gguf(base);
  addInspect(whole, 'valid');
  for (let n = 0; n < whole.length; n += 1) if (n < 40 || n % 7 === 0) addInspect(whole.subarray(0, n), `truncated at ${n}`);
  addInspect(Buffer.alloc(24), 'zeros');
  addInspect(gguf({ magic: Buffer.from([0xc7, 0xc7, 0xd5, 0xc6]) }), 'magic with high bits');
  addInspect(gguf({ magic: Buffer.from('GGUf') }), 'magic case');
  addInspect(gguf({ magic: Buffer.from('FUGG') }), 'big-endian magic');
  for (const version of [0, 1, 2, 3, 4, 0xffffffff]) addInspect(gguf({ ...base, version }), `version ${version}`);
  for (const count of [200000, 200001, 2 ** 53 - 1]) addInspect(gguf({ count }), `count ${count}`);
  for (const kvCount of [100000, 100001]) addInspect(gguf({ kvCount }), `kv ${kvCount}`);
  for (const big of [2n ** 53n - 1n, 2n ** 53n, 2n ** 64n - 1n]) {
    addInspect(Buffer.concat([Buffer.from('GGUF'), u32(3), u64(big), u64(0)]), `tensor count ${big}`);
    addInspect(Buffer.concat([Buffer.from('GGUF'), u32(3), u64(0), u64(1), u64(big)]), `key length ${big}`);
    addInspect(gguf({ kv: [{ key: 'qa.big', type: 10, value: big }, { key: 'general.architecture', type: 10, value: big }] }), `kept u64 ${big}`);
    addInspect(gguf({ tensors: [T('mtp.0', { shape: [Number(big & 0xffffn), 1] })] }), 'shape');
    addInspect(Buffer.concat([gguf({ count: 1 }), gstr('mtp.big'), u32(1), u64(big), u32(0), u64(0)]), `dimension ${big}`);
    addInspect(Buffer.concat([gguf({ count: 1 }), gstr('mtp.big'), u32(0), u32(0), u64(big)]), `offset ${big}`);
  }
  for (const dims of [0, 8, 9, 0xffffffff]) addInspect(Buffer.concat([gguf({ count: 1 }), gstr('mtp.dims'), u32(dims), ...Array.from({ length: Math.min(dims, 9) }, () => u64(1)), u32(0), u64(0)]), `dims ${dims}`);
  for (let type = 0; type <= 14; type += 1) {
    const value = type === 8 ? 'qa' : type === 9 ? { t: 0, items: [1, 2] } : type === 13 || type === 14 ? Buffer.alloc(8) : 1;
    for (const key of ['general.architecture', 'qa.nextn_predict_layers', 'split.count', 'qa.other']) addInspect(gguf({ kv: [{ key, type, value }] }), `type ${type} ${key}`);
  }
  for (const [n, type] of [[5, 0], [-1, 5], [65535, 2], [-2, 3], [2.5, 6], [1, 7], [-3, 1], [2, 11], [1.5, 12], [7, 4]]) {
    addInspect(gguf({ kv: [LAYERS(n, type), SPLIT(n, type)] }), `layers ${n} as ${type}`);
  }
  addInspect(gguf({ kv: [{ key: 'qa.arr', type: 9, value: { t: 9, items: [{ t: 9, items: [] }] } }] }), 'nested depth 2 empty');
  addInspect(gguf({ kv: [{ key: 'qa.arr', type: 9, value: { t: 9, items: [{ t: 9, items: [{ t: 0, items: [1] }] }] } }] }), 'nested depth 3');
  addInspect(gguf({ kv: [{ key: 'qa.arr', type: 9, value: { t: 9, items: [{ t: 0, items: [1, 2] }] } }] }), 'nested depth 1');
  addInspect(gguf({ kv: [{ key: 'qa.arr', type: 9, value: { t: 0, n: 2000000, items: [] } }] }), 'array 2e6 short');
  addInspect(gguf({ kv: [{ key: 'qa.arr', type: 9, value: { t: 0, n: 2000001, items: [] } }] }), 'array over 2e6');
  addInspect(gguf({ kv: [{ key: 'qa.arr', type: 9, value: { t: 0, items: Array.from({ length: 3000 }, (_, i) => i & 0xff) } }] }), 'array long');
  addInspect(gguf({ kv: [{ key: 'general.architecture', type: 9, value: { t: 8, items: ['a', 'b'] } }] }), 'architecture array');
  addInspect(gguf({ kv: [{ key: 'qa.arr', type: 9, value: { t: 13, items: [Buffer.alloc(1)] } }] }), 'array of an unknown type');
  addInspect(gguf({ kv: [{ key: 'qa.arr', type: 9, value: { t: 13, items: [] } }] }), 'empty array of an unknown type');
  addInspect(gguf({ kv: [ARCH('first'), ARCH('second'), LAYERS(2), LAYERS(0), SPLIT(3), SPLIT('4', 8)] }), 'repeated keys');
  addInspect(gguf({ kv: [{ key: Buffer.from([0x67, 0xff, 0x2e, 0x6e]), type: 0, value: 1 }, { key: 'x.nextn_predict_layers', type: 8, value: Buffer.from([0x00]) }] }), 'odd key bytes');
  addInspect(gguf({ kv: [{ key: 'general.architecture', type: 8, value: Buffer.from([0xe2, 0x82, 0x28, 0xed, 0xa0, 0x80, 0xf0, 0x9f, 0x98, 0x80, 0xc0, 0xaf]) }] }), 'invalid UTF-8 architecture');
  addInspect(gguf({ tensors: NAMES.map((n) => T(n)) }), 'every name');
  addInspect(gguf({ tensors: Array.from({ length: 20 }, (_, i) => T(`blk.${i}.nextn.w`)) }), 'over eight');
  addInspect(gguf({ tensors: [T(Buffer.from([0x6d, 0x74, 0x70, 0x5f, 0xff])), T(Buffer.from([0xff, 0x2e, 0x6d, 0x74, 0x70, 0x2e])), T(Buffer.from([0xc3, 0x2e, 0x4e, 0x65, 0x58, 0x74, 0x4e, 0x5f]))] }), 'byte names');
  addInspect(gguf({ tensors: [T('mtp.0', { shape: [] })] }), 'scalar tensor');
  addInspect(Buffer.concat([gguf(base), Buffer.alloc(100, 0x41)]), 'trailing bytes');
}

function randomGguf() {
  const kv = [];
  const nk = int(6);
  for (let i = 0; i < nk; i += 1) {
    const key = pick(['general.architecture', 'qa.nextn_predict_layers', 'split.count', 'general.name', 'qa.context_length', 'tokenizer.ggml.tokens', 'é.ünïcode']);
    const type = pick([0, 1, 2, 3, 4, 5, 6, 7, 8, 8, 9, 10, 11, 12, 13]);
    const value = type === 8 ? pick(['qa_arch', '', '2', ' 3 ', '0x10', 'Infinity', 'é😀', '-1'])
      : type === 9 ? { t: pick([0, 4, 8, 9, 13]), items: [] } : type === 13 ? Buffer.alloc(int(9)) : type === 10 ? BigInt(int(5)) * pick([1n, 2n ** 40n]) : type === 11 ? BigInt(int(9) - 4) : pick([0, 1, 2, 3, 200, -1]);
    if (type === 9) {
      const t = value.t; const n = int(4);
      for (let j = 0; j < n; j += 1) value.items.push(t === 8 ? 'qa' : t === 9 ? { t: 0, items: rand() < 0.5 ? [] : [1] } : t === 13 ? Buffer.alloc(0) : 1);
    }
    kv.push({ key, type, value });
  }
  const tensors = Array.from({ length: int(12) }, () => T(pick(NAMES), { shape: Array.from({ length: int(4) }, () => 1 + int(9000)) }));
  let bytes = gguf({ version: pick([3, 3, 3, 2, 1]), kv, tensors });
  const r = rand();
  if (r < 0.2) bytes = bytes.subarray(0, int(bytes.length + 1));
  else if (r < 0.35) { bytes = Buffer.from(bytes); for (let k = 0; k < 1 + int(3); k += 1) bytes[int(bytes.length)] = int(256); }
  return bytes;
}

// ── deriveUserModelName ─────────────────────────────────────────────────────────────────────
const NAME_INPUTS = ['qa-org/QA-Model-7B-GGUF:Q4_K_M', 'qa-org/QA-Model', 'QA-Model', 'qa/', '/', '', ':', 'a:', ':b', 'a:b:c', 'qa/é😀:Q4 K',
  'qa/model name:var/iant', 'qa\\x/y', 'qa/\ud800', 'qa/a\u0000b', 5, 0, true, null, [1, [2, 3]], ['qa/x', 'y'], {}, { a: 1 }, 1e21, -0.5, 'qa/x::', 'qa//x'];
const names = NAME_INPUTS.map((input) => {
  try { return { input, out: deriveUserModelName(input) }; } catch (e) { return { input, error: e.constructor.name }; }
});

// ── llamacpp-variants.cjs ───────────────────────────────────────────────────────────────────
const QUANT_FILES = ['qa-Q4_K_M.gguf', 'qa-q4_k_m.gguf', 'qa.IQ2_XS.gguf', 'QA_BF16.gguf', 'qa-F16.gguf', 'qa-f32.GGUF', 'qa-mmproj-F16.gguf', 'MMPROJ-Q8_0.gguf',
  'qa-Q8_0-00001-of-00002.gguf', 'qa-Q8_0-00002-of-00002.gguf', 'qa-Q6_K-00001-of-00003.gguf', 'qa-Q6_K-00002-of-00003.gguf', 'sub/dir/qa-Q5_K_S.gguf',
  'qa-Q5_K_S.gguf', 'qa-Q.gguf', 'qa-Q4.gguf.bak', 'Q3_K_L.gguf', 'qaQ4_0.gguf', 'qa-Q2_K-00001-of-00001.gguf', 'other-Q8_0-00001-of-00002.gguf',
  'qa-IQ4_NL-00003-of-00002.gguf', 'qa-IQ4_NL-00000-of-00002.gguf', 'qa-Q4_0_4_4.gguf', 'qa-é-Q4_1.gguf', 'qa.Q4.b_Q5_K.gguf', 'qa-F16-00001-OF-00002.GGUF',
  'qa-F16-00002-of-00002.gguf', 'qa-BF16-1-of-2.gguf', 'readme.md', 'qa-Q4_K_M.gguf.part', 'qa-Q4-K.gguf'];
const quants = [];
async function addQuants(siblings, extra = {}) {
  const body = clean({ siblings, ...extra });
  const out = await llamacppVariants('qa-org/qa-model', async () => ({ ok: true, status: 200, body: viaJson(body) }))
    .then((r) => ({ out: viaJson(r) }), (e) => ({ error: e.constructor.name }));
  quants.push({ body: viaJson(body), ...out });
}

// ── Managers ────────────────────────────────────────────────────────────────────────────────
let fileNo = 0;
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-model-pull-'));
function buildManager(c, calls) {
  const script = (c.script || []).slice();
  const fetchJson = async (url, init, timeout) => {
    calls.push({ send: clean({ url, method: init?.method || 'GET', headers: init?.headers || {}, body: init?.body, timeout }) });
    const next = script.shift();
    if (!next) { problems.push(`unscripted manager call ${url}`); throw Error('qa: unscripted'); }
    if (next.throw) throw Object.assign(Error(next.throw.message), next.throw.status === undefined ? {} : { status: next.throw.status });
    return viaJson(next.reply);
  };
  if (c.kind !== 'llamacpp') return { manager: createModelManager({ kind: c.kind, baseUrl: c.base, apiKey: c.apiKey, fetchJson }), file: null };
  const file = path.join(tmp, `downloads-${fileNo += 1}.json`);
  const manager = createModelManager({ kind: 'llamacpp', baseUrl: c.base, apiKey: c.apiKey, fetchJson, downloadStatePath: file,
    fetchStream: async (url) => { calls.push({ sse: url }); return { ok: false, status: 503 }; },
    // The pull never lists models; this keeps dav-parse.wasm (the long-profile pairing) out.
    autotuneOptions: { longProfiles: { pairsOf: () => [] } } });
  if (c.barrier) manager.chatBarrier(c.barrier);
  return { manager, file };
}
function downloadsOf(file) {
  if (!file) return undefined;
  try { return JSON.parse(fs.readFileSync(file, 'utf8')).map((j) => ({ model: j.model, status: j.status })); } catch { return []; } finally { fs.rmSync(file, { force: true }); }
}

async function callRoute({ method, p, role, query, body, manager }) {
  const out = {};
  const chunks = body === undefined ? [] : [body];
  const reqStub = { method, async *[Symbol.asyncIterator]() { for (const c of chunks) yield c; } };
  const jsonStub = (r, status, b) => { out.status = status; out.body = viaJson(b); };
  const service = { invalidateModelReads: () => { out.invalidated = (out.invalidated || 0) + 1; } };
  const url = new URL(query === undefined ? p : `${p}?${query}`, 'http://localhost');
  try {
    const handled = await route(reqStub, {}, { path: p, authn: { user: { id: 'qa-user', role } }, url }, jsonStub, readBody, manager, service, routeRequire);
    if (!handled) out.unhandled = true;
  } catch (err) {
    // index.cjs: a rejection is answered with errorResponse.
    const failure = errorResponse(err);
    Object.assign(out, { thrown: true, status: failure.status, body: viaJson(failure.body) });
  }
  return out;
}

// ── POST /api/models/pull ───────────────────────────────────────────────────────────────────
const pullCases = [];
const padded = (total) => { const head = '{"checkpoint":"qa-org/qa-model:Q4_K_M","pad":"', tail = '"}'; return head + 'x'.repeat(total - head.length - tail.length) + tail; };
const bytesOf = (c) => (c.base64 !== undefined ? Buffer.from(c.base64, 'base64') : c.padded !== undefined ? Buffer.from(padded(c.padded), 'utf8') : c.body === undefined ? undefined : Buffer.from(c.body, 'utf8'));
async function addPull(c) {
  const calls = [];
  const { manager, file } = buildManager(c, calls);
  const out = await callRoute({ method: c.method, p: '/api/models/pull', role: c.role, body: bytesOf(c), manager });
  pullCases.push({ ...clean({ kind: c.kind, base: c.base, apiKey: c.apiKey, barrier: c.barrier, method: c.method, role: c.role, request: c.body,
    requestBase64: c.base64, requestPadded: c.padded, script: c.script }), ...out, calls, ...clean({ downloads: downloadsOf(file) }) });
}
const ok = (body, status = 200) => ({ reply: clean({ ok: true, status, body }) });
const fail = (status, body) => ({ reply: clean({ ok: false, status, body }) });
const LEMONADE = { kind: 'lemonade', base: 'http://qa-manager.invalid//', apiKey: 'qa-api-key' };
const LLAMACPP = { kind: 'llamacpp', base: 'http://qa-router.invalid/v1/', apiKey: 'qa-router-key' };
const NONE = { kind: 'none', base: '', apiKey: 'local' };
const PULL_BODIES = ['{"checkpoint":"qa-org/qa-model:Q4_K_M"}', '{"checkpoint":"qa-org/qa-model"}', '{"checkpoint":"qa-org/qa-model:Q4_K_M","modelName":"qa-name","recipe":"qa-recipe"}',
  '{"checkpoint":"qa-org/qa-model","modelName":"","recipe":""}', '{"checkpoint":"qa-org/qa-model","modelName":0,"recipe":false}', '{"checkpoint":"qa-org/qa-model","modelName":{"qa":1},"recipe":[1]}',
  '{"checkpoint":""}', '{"checkpoint":0}', '{"checkpoint":null}', '{"checkpoint":false}', '{}', '[]', '"qa"', '5', 'null', 'true', '', ' ', '{not json',
  '{"checkpoint":5}', '{"checkpoint":true}', '{"checkpoint":["qa-org/qa-model"]}', '{"checkpoint":["qa-org","qa-model"]}', '{"checkpoint":{"qa":1}}',
  '{"checkpoint":{"toString":1}}', '{"checkpoint":{"toString":1},"modelName":"qa-name"}', '{"checkpoint":[[1,[2]],null]}', '{"checkpoint":"qa-org/QA-é😀:Q4 K"}',
  '{"checkpoint":"qa-org/qa\\ud800"}', '{"checkpoint":"qa-org/qa-model:"}', '{"checkpoint":"qa-org/qa-model:a:b"}', '{"checkpoint":"/qa-model"}',
  '{"checkpoint":"qa-org/qa.model-1_x:IQ2_XS"}', '{"checkpoint":"qa org/qa"}', '{"checkpoint":"qa-org/qa-model","checkpoint":"qa-org/second"}',
  '﻿{"checkpoint":"qa-org/qa-model"}', '{"checkpoint":"qa-org/qa-model"}\u0000', ' {"checkpoint":"qa-org/qa-model"} ', '{"checkpoint":"qa-org/qa-model"}x',
  '{"checkpoint":1e400}', '{"checkpoint":"qa-org/qa-model","n":1e400}', '{"checkpoint":"qa-org/qa-model","__proto__":{"x":1}}', '{"checkpoint":"\\u0000"}',
  '{"checkpoint":-1}', '{"checkpoint":"0"}', '{"checkpoint":"qa-org/qa-model:Q4_K_M\\n"}'];
const PULL_REPLIES = [[ok({ id: 'qa-job-1', modelName: 'qa-served' })], [ok({ job_id: 'qa-job-2' })], [ok({ id: '', job_id: '', modelName: '' })], [ok({ id: 0, job_id: 7 })],
  [ok(null)], [ok('qa text')], [ok({ id: { qa: 1 }, modelName: ['qa'] })], [ok({}, 201)], [ok({ error: 'qa ignored' }, 299)], [fail(500, { error: 'qa manager failed' })],
  [fail(404, {})], [fail(400, { error: '' })], [fail(409, 'qa text')], [fail(502, { error: { qa: 1 } })], [fail(503)], [{ reply: { ok: false, status: 200, error: 'response too large' } }],
  [{ throw: { message: 'qa: fetch failed' } }], [{ throw: { message: 'qa: lease lost', status: 503 } }], [{ throw: { message: 'qa: refused', status: 409 } }],
  [{ throw: { message: '', status: 404 } }], [{ throw: { message: 'qa', status: 600 } }], [{ throw: { message: 'qa', status: 399 } }]];
async function pulls() {
  for (const method of ['POST', 'PUT', 'GET', 'DELETE', 'HEAD', 'PATCH']) for (const role of ['admin', 'member']) {
    await addPull({ ...LEMONADE, method, role, body: PULL_BODIES[0], script: PULL_REPLIES[0] });
  }
  for (const kind of [NONE, LEMONADE, LLAMACPP, { ...LEMONADE, base: 'http://qa-manager.invalid/qa/', apiKey: 'local' }, { ...LEMONADE, apiKey: '' },
    { ...LLAMACPP, base: 'http://qa-router.invalid', apiKey: 'local' }, { ...LLAMACPP, base: 'http://qa-router.invalid/v1/v1' }]) {
    for (const body of PULL_BODIES) await addPull({ ...kind, method: 'POST', role: 'admin', body, script: PULL_REPLIES[0] });
  }
  for (const kind of [LEMONADE, LLAMACPP]) for (const script of PULL_REPLIES) await addPull({ ...kind, method: 'POST', role: 'admin', body: PULL_BODIES[0], script });
  await addPull({ ...LLAMACPP, method: 'POST', role: 'admin', body: PULL_BODIES[0], barrier: 'QA maintenance is running ✓', script: [] });
  await addPull({ ...LLAMACPP, method: 'POST', role: 'admin', body: '{"checkpoint":"not valid"}', barrier: 'QA maintenance is running', script: [] });
  // Raw bytes: invalid UTF-8 in a string (read as U+FFFD) and in a number (invalid JSON); the 1 MiB edge.
  const raw = [Buffer.concat([Buffer.from('{"checkpoint":"qa-org/qa-'), Buffer.from([0xff, 0xc3, 0x28]), Buffer.from('"}')]),
    Buffer.concat([Buffer.from('{"checkpoint":1'), Buffer.from([0xff]), Buffer.from('2}')]), Buffer.from([0xef, 0xbb, 0xbf, 0x7b, 0x7d])];
  for (const b of raw) for (const kind of [LEMONADE, LLAMACPP]) await addPull({ ...kind, method: 'POST', role: 'admin', base64: b.toString('base64'), script: PULL_REPLIES[0] });
  for (const total of [1024 * 1024 - 1, 1024 * 1024, 1024 * 1024 + 1]) await addPull({ ...LEMONADE, method: 'POST', role: 'admin', padded: total, script: PULL_REPLIES[0] });
  for (let n = 0; n < 400; n += 1) {
    const kind = pick([NONE, LEMONADE, LEMONADE, LLAMACPP, LLAMACPP]);
    const body = rand() < 0.5 ? JSON.stringify(clean({ checkpoint: pick(['qa-org/qa-model:Q4_K_M', 'qa-org/qa-model', 'qa-org/Ünï:Q8_0', 'bad', 5, ['qa/x']]),
      modelName: pick([undefined, 'qa-name', '', 0]), recipe: pick([undefined, 'qa-recipe', null]) })) : pick(PULL_BODIES);
    await addPull({ ...kind, method: 'POST', role: rand() < 0.9 ? 'admin' : 'member', body, script: pick(PULL_REPLIES),
      barrier: kind === LLAMACPP && rand() < 0.1 ? 'QA maintenance' : undefined });
  }
}

// ── GET /api/models/mtp-artifact ────────────────────────────────────────────────────────────
// A fetch answer: { throw: 'timeout' | 'abort' | 'network' } or { status, contentRange?, json? | text? | parts?, chunk?, error? }.
function domError(kind) {
  if (kind === 'timeout') return new DOMException('The operation was aborted due to timeout', 'TimeoutError');
  if (kind === 'abort') return new DOMException('This operation was aborted', 'AbortError');
  return new TypeError(kind === 'network' ? 'fetch failed' : kind);
}
function bodyBytes(o) {
  if (o.json !== undefined) return Buffer.from(JSON.stringify(o.json), 'utf8');
  if (o.text !== undefined) return Buffer.from(o.text, 'utf8');
  if (o.parts !== undefined) return fromParts(o.parts);
  return Buffer.alloc(0);
}
function respond(o) {
  if (o.throw) throw domError(o.throw);
  const nullBody = [204, 205, 304].includes(o.status);
  const headers = o.contentRange === undefined ? {} : { 'content-range': o.contentRange };
  if (nullBody) return new Response(null, { status: o.status, headers });
  const bytes = bodyBytes(o);
  const chunk = o.chunk || 65536;
  let at = 0;
  const stream = new ReadableStream({
    pull(c) {
      if (at < bytes.length) { c.enqueue(bytes.subarray(at, at + chunk)); at += chunk; return; }
      if (o.error) c.error(domError(o.error)); else c.close();
    },
  }, { highWaterMark: 0 });
  return new Response(stream, { status: o.status, headers });
}
const REV = 'a'.repeat(8) + '0123456789abcdef'.repeat(2);
const META = (sha = REV) => ({ status: 200, json: { id: 'qa-org/qa-model', sha, private: false } });
const metaUrl = (repo) => `https://huggingface.co/api/models/${repo}`;
const fileUrl = (repo, rev, file) => `https://huggingface.co/${repo}/resolve/${rev}/${file.split('/').map(encodeURIComponent).join('/')}`;
const ggufAnswer = (bytes, extra = {}) => ({ status: 206, contentRange: `bytes 0-${bytes.length - 1}/99999999999`, parts: toParts(bytes), ...extra });
const MTP_GGUF = gguf({ kv: [ARCH('qa_arch'), LAYERS(1)], tensors: [T('blk.0.attn_q.weight'), T('blk.40.nextn.eh_proj.weight')] });
const PLAIN_GGUF = gguf({ kv: [ARCH('qa_arch')], tensors: [T('blk.0.attn_q.weight')] });
const DECLARED_GGUF = gguf({ kv: [ARCH('qa_arch'), LAYERS(1)], tensors: [T('blk.0.attn_q.weight')] });
const SPLIT_GGUF = (n, type) => gguf({ kv: [ARCH('qa_arch'), SPLIT(n, type)], tensors: [T('blk.0.mtp.w')] });
const lemonadeVariants = (rows) => [ok({ suggested_name: 'qa', variants: rows })];
const ROW = (name, files, primary = files?.[0]) => clean({ name, primary_file: primary, files });

const mtpScenarios = [];
async function scenario(name, steps) {
  freshMtp();
  const record = { name, steps: [] };
  for (const step of steps) {
    clock = step.at ?? clock;
    const fetches = [];
    let release;
    const gate = new Promise((r) => { release = r; });
    const wave = step.requests.length > 1;
    globalThis.fetch = async (url, init) => {
      fetches.push(clean({ url: String(url), range: init?.headers?.Range, signal: init?.signal ? true : undefined }));
      if (wave) await gate;
      const o = step.hf?.[url];
      if (!o) { problems.push(`${name}: unscripted fetch ${url}`); throw new TypeError('fetch failed'); }
      return respond(o);
    };
    const running = step.requests.map((r) => {
      const calls = [];
      const { manager } = buildManager({ ...LEMONADE, ...r }, calls);
      return callRoute({ method: r.method || 'GET', p: '/api/models/mtp-artifact', role: r.role || 'member', query: r.query, manager }).then((out) => ({ r, out, calls }));
    });
    for (let i = 0; i < 20; i += 1) await new Promise((r) => setImmediate(r));
    release();
    const done = await Promise.all(running);
    globalThis.fetch = async (url) => { problems.push(`${name}: fetch after the step ${url}`); throw TypeError('fetch failed'); };
    record.steps.push(clean({ at: clock, hf: step.hf, wave: wave || undefined,
      requests: done.map(({ r, out, calls }) => ({ ...clean({ kind: r.kind || LEMONADE.kind, base: r.base || LEMONADE.base, apiKey: r.apiKey ?? LEMONADE.apiKey,
        method: r.method || 'GET', role: r.role || 'member', query: r.query, script: r.script }), ...out, calls })),
      fetches: wave ? fetches.map((f) => JSON.stringify(f)).sort().map((f) => JSON.parse(f)) : fetches }));
  }
  mtpScenarios.push(record);
}
// One request, one step.
const one = (name, request, hf, at) => scenario(name, [{ at, requests: [request], hf }]);
const happyHf = (repo, files, answers) => ({ [metaUrl(repo)]: META(), ...Object.fromEntries(files.map((f, i) => [fileUrl(repo, REV, f), answers[i]])) });

async function mtps() {
  const repo = 'qa-org/qa-model';
  const files1 = ['qa-Q4_K_M.gguf'];
  const q = (variant = 'Q4_K_M', r = repo) => `repo=${encodeURIComponent(r)}&variant=${encodeURIComponent(variant)}`;
  const lem = (rows) => ({ script: lemonadeVariants(rows) });
  const base = (extra = {}) => ({ query: q(), ...lem([ROW('Q4_K_M', files1)]), ...extra });
  const hf1 = (answer) => happyHf(repo, files1, [answer]);

  // Methods and roles (a member may GET; any other method is a write: the prelude, then not this route).
  for (const method of ['GET', 'HEAD', 'POST', 'PUT']) for (const role of ['admin', 'member']) {
    await one(`method ${method} ${role}`, base({ method, role }), method === 'GET' ? hf1(ggufAnswer(MTP_GGUF)) : {});
  }
  // Queries.
  for (const query of [undefined, '', 'repo=', 'repo=qa', 'repo=qa/', 'repo=/qa', 'repo=qa/a/b', 'repo=qa%2Fx', 'repo=qa/x+y', 'repo=qa/%C3%A9', 'repo=qa/é',
    'repo=qa/x&repo=qa/y', 'variant=Q4_K_M&repo=qa-org/qa-model', 'repo=qa-org/qa-model%00', 'repo=qa-org/qa-model ', 'repo=qa-org/qa-model\t', 'repo=qa-org/qa-model#x',
    'repo=qa-org/qa-model%23x', 'repo=qa-org/qa-model&variant=q4_k_m', 'repo=qa-org/qa-model&variant=Q4_K_M%20', 'repo=qa-org/qa-model&variant=Q4+K', 'repo=qa-org/qa-model&variant',
    'repo=qa-org/qa-model&variant=', 'repo=qa-org/qa-model&variant=%FF', 'repo=./x', 'repo=a/..', 'repo=..%2F..', 'REPO=qa-org/qa-model', 'repo=qa-org/qa-model&&variant=Q4_K_M&']) {
    const rows = [ROW('Q4_K_M', files1), ROW('q4_k_m', ['qa-q4.gguf']), ROW('Q4 K', ['qa-q4k.gguf']), ROW('', ['qa-empty.gguf']), ROW('�', ['qa-ff.gguf']), ROW('Q4_K_M ', ['qa-sp.gguf']), ROW(null, ['qa-null.gguf'])];
    const repoOf = new URL(`/x?${query ?? ''}`, 'http://localhost').searchParams.get('repo') || '';
    await one(`query ${JSON.stringify(query)}`, { query, script: lemonadeVariants(rows) }, {
      [metaUrl(repoOf)]: META(), ...Object.fromEntries(['qa-Q4_K_M.gguf', 'qa-q4.gguf', 'qa-q4k.gguf', 'qa-empty.gguf', 'qa-ff.gguf', 'qa-sp.gguf', 'qa-null.gguf'].map((f) => [fileUrl(repoOf, REV, f), ggufAnswer(PLAIN_GGUF)])) });
  }
  // Kinds: none (variants throws), and the lemonade manager's answers.
  await one('kind none', { ...base(), kind: 'none', base: '', apiKey: 'local', script: [] }, {});
  for (const script of [[ok({ variants: [] })], [ok(null)], [ok('qa text')], [ok({ variants: 'qa' })], [ok({ variants: { qa: 1 } })], [ok({ variants: 0 })], [ok({ variants: 5 })],
    [ok({ variants: [null, ROW('Q4_K_M', files1)] })], [ok({ variants: [ROW('Q4_K_M', files1), null] })], [ok({ variants: [5, 'Q4_K_M', ['Q4_K_M'], ROW('Q4_K_M', files1)] })],
    [ok({ variants: [ROW('Q4_K_M', files1), ROW('Q4_K_M', ['qa-second.gguf'])] })], [fail(500, { error: 'qa' })], [fail(404)], [{ reply: { ok: false, status: 200, error: 'response too large' } }],
    [{ throw: { message: 'qa: fetch failed' } }], [{ throw: { message: 'qa: refused', status: 403 } }], [ok({ variants: [{ name: 'Q4_K_M', primary_file: 'qa-Q4_K_M.gguf' }] })],
    [ok({ variants: [{ name: 'Q4_K_M', files: [], primary_file: 'qa-Q4_K_M.gguf' }] })], [ok({ variants: [{ name: 'Q4_K_M', files: null }] })], [ok({ variants: [{ name: 'Q4_K_M' }] })],
    [ok({ variants: [{ name: 'Q4_K_M', files: 'qa-Q4_K_M.gguf' }] })], [ok({ variants: [{ name: 'Q4_K_M', files: { a: 'qa-Q4_K_M.gguf' } }] })], [ok({ variants: [{ name: 'Q4_K_M', files: 7 }] })],
    [ok({ variants: [{ name: 'Q4_K_M', files: [7] }] })], [ok({ variants: [{ name: 'Q4_K_M', files: ['qa-Q4_K_M.gguf', 'qa-Q4_K_M.gguf'] }] })]]) {
    await one(`lemonade ${JSON.stringify(script)}`, { query: q(), script }, hf1(ggufAnswer(MTP_GGUF)));
  }
  await one('variant null name, no variant param', { query: `repo=${repo}`, script: [ok({ variants: [ROW(null, files1)] })] }, hf1(ggufAnswer(MTP_GGUF)));
  await one('lemonade base and key', { ...base(), base: 'http://qa-manager.invalid/qa/', apiKey: '' }, hf1(ggufAnswer(MTP_GGUF)));
  // Files the check refuses.
  for (const files of [[], ['a.gguf', 'b.gguf', 'c.gguf', 'd.gguf', 'e.gguf'], ['a.gguf', 'a.gguf'], ['a.bin'], ['/a.gguf'], ['d//a.gguf'], ['./a.gguf'], ['d/../a.gguf'], ['a?.gguf'],
    ['a#.gguf'], ['a\\b.gguf'], ['a\u0001.gguf'], ['a\u007f.gguf'], ['d/'], ['.gguf'], [null], [true], ['a.GGUF'], ['a.gguf ']]) {
    const hf = happyHf(repo, files.filter((f) => typeof f === 'string'), files.map(() => ggufAnswer(PLAIN_GGUF)));
    await one(`files ${JSON.stringify(files)}`, { query: q(), script: lemonadeVariants([ROW('Q4_K_M', files)]) }, hf);
  }
  // Four files, names that need encoding, a lone surrogate (URIError).
  const four = ['qa-00001-of-00004.gguf', 'qa-00002-of-00004.gguf', 'qa-00003-of-00004.gguf', 'qa-00004-of-00004.gguf'];
  await one('four shards', { query: q(), script: lemonadeVariants([ROW('Q4_K_M', four)]) }, happyHf(repo, four, four.map((_, i) => ggufAnswer(i === 2 ? MTP_GGUF : SPLIT_GGUF(4)))));
  const odd = ['sub dir/é 😀+%.gguf', "q!~*'()&=.gguf"];
  await one('encoded names', { query: q(), script: lemonadeVariants([ROW('Q4_K_M', odd)]) }, happyHf(repo, odd, odd.map(() => ggufAnswer(PLAIN_GGUF))));
  await one('lone surrogate name', { query: q(), script: lemonadeVariants([ROW('Q4_K_M', ['qa-ok.gguf', 'qa-\ud800.gguf'])]) },
    { [metaUrl(repo)]: META(), [fileUrl(repo, REV, 'qa-ok.gguf')]: ggufAnswer(PLAIN_GGUF) });
  // The revision reply.
  for (const meta of [{ status: 404, json: {} }, { status: 500, json: {} }, { status: 302, json: {} }, { status: 204 }, { status: 205 }, { status: 200, text: '' }, { status: 200, text: ' \n\t' },
    { status: 200, text: '{not json' }, { status: 200, text: '﻿{}' }, { status: 200, json: null }, { status: 200, json: 5 }, { status: 200, json: 'qa' }, { status: 200, json: [] },
    { status: 200, json: {} }, META(''), META(0), META(null), META(REV.toUpperCase()), META(REV.slice(0, 39)), META('f'.repeat(64)), META('f'.repeat(65)), META(`${REV}\n`),
    META([REV]), META([[REV]]), META([REV, REV]), META({ toString: 1 }), META([{ toString: 1 }]), META({}), META(true), META(1e40),
    { status: 200, text: `{"sha":"${REV}","n":1e400}` }, { status: 200, parts: [{ fill: 0x20, n: 4 * 1024 * 1024 - 60 }, { b64: Buffer.from(`{"sha":"${REV}"}`).toString('base64') }] },
    { status: 200, parts: [{ fill: 0x20, n: 4 * 1024 * 1024 }, { b64: Buffer.from(`{"sha":"${REV}"}`).toString('base64') }] },
    { status: 200, parts: [{ b64: Buffer.from(`{"sha":"${REV}"}`).toString('base64') }, { fill: 0x20, n: 4 * 1024 * 1024 - 51 }] },
    { status: 200, parts: [{ b64: Buffer.from(`{"sha":"${REV}"}`).toString('base64') }, { fill: 0x20, n: 4 * 1024 * 1024 - 50 }] },
    { status: 200, parts: [{ b64: Buffer.from(`{"sha":"${REV}"}`).toString('base64') }, { fill: 0x20, n: 4 * 1024 * 1024 - 50 }], chunk: 1024 * 1024, error: 'terminated' },
    { status: 200, json: { sha: REV }, error: 'terminated', chunk: 4 }, { status: 200, json: { sha: REV }, error: 'abort', chunk: 4 }, { throw: 'timeout' }, { throw: 'abort' }, { throw: 'network' }]) {
    await one(`meta ${JSON.stringify(meta).slice(0, 120)}`, base(), { ...hf1(ggufAnswer(MTP_GGUF)), [fileUrl(repo, 'f'.repeat(64), files1[0])]: ggufAnswer(PLAIN_GGUF), [metaUrl(repo)]: meta });
  }
  // The prefix reply.
  for (const prefix of [{ status: 404 }, { status: 500 }, { status: 204 }, { status: 205 }, { status: 200, parts: toParts(MTP_GGUF) }, { status: 206, parts: toParts(MTP_GGUF) },
    { status: 206, contentRange: 'BYTES 0-99/100', parts: toParts(MTP_GGUF) }, { status: 206, contentRange: 'bytes 1-99/100', parts: toParts(MTP_GGUF) },
    { status: 206, contentRange: ' bytes 0-99/100', parts: toParts(MTP_GGUF) }, { status: 206, contentRange: 'bytes 0-', parts: toParts(MTP_GGUF) },
    { status: 201, parts: toParts(MTP_GGUF) }, { status: 299, parts: toParts(PLAIN_GGUF) }, ggufAnswer(MTP_GGUF, { chunk: 3 }), ggufAnswer(MTP_GGUF, { chunk: 10, error: 'terminated' }),
    ggufAnswer(MTP_GGUF, { error: 'terminated' }), ggufAnswer(MTP_GGUF, { chunk: 5, error: 'abort' }), ggufAnswer(MTP_GGUF.subarray(0, 30)), ggufAnswer(Buffer.from('not a gguf at all, qa')),
    { throw: 'timeout' }, { throw: 'abort' }, { throw: 'network' }, ggufAnswer(DECLARED_GGUF), ggufAnswer(PLAIN_GGUF), ggufAnswer(SPLIT_GGUF(2)), ggufAnswer(SPLIT_GGUF(1)),
    ggufAnswer(SPLIT_GGUF('2', 8)), ggufAnswer(SPLIT_GGUF('x', 8)), ggufAnswer(SPLIT_GGUF(' 1 ', 8)), ggufAnswer(SPLIT_GGUF(2, 6)),
    ggufAnswer(gguf({ kv: [LAYERS('1', 8)], tensors: [T('x')] })), ggufAnswer(gguf({ kv: [LAYERS('0x1', 8)], tensors: [T('x')] })), ggufAnswer(gguf({ kv: [LAYERS(-1, 5)], tensors: [T('x')] })),
    ggufAnswer(gguf({ kv: [{ key: 'general.architecture', type: 4, value: 7 }], tensors: [T('mtp.é')] }))]) {
    await one(`prefix ${JSON.stringify(prefix).slice(0, 120)}`, base(), hf1(prefix));
  }
  // The 16 MiB bound: a directory that ends exactly at the limit, one byte past it, a body longer than the limit.
  const longKey = (n) => Buffer.concat([Buffer.from('GGUF'), u32(3), u64(1), u64(1), u64(n), Buffer.alloc(n, 0x6b), u32(0), Buffer.from([1]), gstr('blk.0.nextn.w'), u32(1), u64(1), u32(0), u64(0)]);
  const tailLen = 4 + 1 + gstr('blk.0.nextn.w').length + 4 + 8 + 4 + 8;
  const exact = LIMIT - 32 - tailLen;
  for (const [n, extra] of [[exact, 0], [exact + 1, 0], [exact, 4096], [exact - 1, 1 << 20]]) {
    const bytes = Buffer.concat([longKey(n), Buffer.alloc(extra, 0x5a)]);
    await one(`limit key ${n} + ${extra}`, base(), hf1(ggufAnswer(bytes, { chunk: 1 << 20 })));
  }
  // Two files: the second fails, the first's split count.
  const two = ['qa-a.gguf', 'qa-b.gguf'];
  for (const answers of [[ggufAnswer(PLAIN_GGUF), { status: 404 }], [ggufAnswer(SPLIT_GGUF(3)), ggufAnswer(PLAIN_GGUF)], [ggufAnswer(DECLARED_GGUF), ggufAnswer(PLAIN_GGUF)],
    [ggufAnswer(DECLARED_GGUF), ggufAnswer(MTP_GGUF)], [ggufAnswer(PLAIN_GGUF), ggufAnswer(PLAIN_GGUF)]]) {
    await one(`two ${JSON.stringify(answers.map((a) => a.status))}`, { query: q(), script: lemonadeVariants([ROW('Q4_K_M', two)]) }, happyHf(repo, two, answers));
  }
  // The llama.cpp manager's variants (Hugging Face's sibling list read by llamacpp-variants.cjs).
  const hfList = (siblings, extra = {}) => [ok({ siblings, ...extra })];
  const sib = (names) => names.map((rfilename) => ({ rfilename, size: 1 }));
  const shards = ['qa-Q8_0-00001-of-00002.gguf', 'qa-Q8_0-00002-of-00002.gguf'];
  for (const [variant, script] of [['Q8_0', hfList(sib(shards))], ['Q8_0', hfList(sib([shards[0]]))], ['Q4_K_M', hfList(sib(QUANT_FILES))], ['Q8_0', hfList(sib(QUANT_FILES))],
    ['IQ2_XS', hfList(sib(QUANT_FILES))], ['BF16', hfList(sib(QUANT_FILES))], ['F16', hfList(sib(QUANT_FILES))], ['Q5_K_S', hfList(sib(QUANT_FILES))], ['Q4_K_M', hfList(sib(QUANT_FILES), { private: true })],
    ['Q4_K_M', hfList(null)], ['Q4_K_M', hfList([null])], ['Q4_K_M', hfList([5, 'x', { rfilename: 7 }, ...sib(['qa-Q4_K_M.gguf'])])], ['Q4_K_M', [fail(404, {})]], ['Q4_K_M', [ok('qa')]],
    ['Q4_K_M', [{ throw: { message: 'qa: fetch failed' } }]], ['q4_k_m', hfList(sib(QUANT_FILES))], [undefined, hfList(sib(QUANT_FILES))]]) {
    const files = variant === 'Q8_0' ? shards : ['qa-Q4_K_M.gguf', 'qa.IQ2_XS.gguf', 'QA_BF16.gguf', 'qa-F16.gguf', 'sub/dir/qa-Q5_K_S.gguf', 'qa-Q5_K_S.gguf'];
    const query = variant === undefined ? `repo=${repo}` : q(variant);
    await one(`llamacpp ${variant} ${JSON.stringify(script).slice(0, 100)}`, { ...LLAMACPP, query, script },
      { [metaUrl(repo)]: META(), ...Object.fromEntries(files.map((f) => [fileUrl(repo, REV, f), ggufAnswer(f.includes('Q8') ? MTP_GGUF : PLAIN_GGUF)])) });
  }
  await one('llamacpp repo a/..', { ...LLAMACPP, query: 'repo=a/..&variant=Q4_K_M', script: hfList(sib(['qa-Q4_K_M.gguf'])) }, {});
  for (let n = 0; n < 40; n += 1) {
    const list = Array.from({ length: 1 + int(8) }, () => pick(QUANT_FILES));
    await addQuants(sib(list));
  }
  for (const siblings of [sib(QUANT_FILES), [], [null], [5, 'x', {}], sib(shards), sib([...shards, shards[0]]), sib(['a-Q8_0-00001-of-00002.gguf', 'b-Q8_0-00002-of-00002.gguf']),
    sib(['qa-Q8_0-00001-of-00002.gguf', 'qa-Q8_0-00003-of-00002.gguf']), sib(['qa-Q8_0-00001-of-00002.gguf', 'qa-Q8_0.gguf']), sib(['qa-Q8_0.gguf', 'qa-Q8_0-00001-of-00002.gguf']),
    sib(['qa-Q8_0-00002-of-00002.gguf', 'qa-Q8_0-00001-of-00002.gguf']), sib(['qa-Q8_0-00001-of-00002.gguf', 'qa-Q8_0-00002-of-00003.gguf'])]) await addQuants(siblings);
  await addQuants(sib(['qa-Q4_K_M.gguf']), { private: 1 });
  await addQuants('qa');

  // The cache: the same check within five minutes is answered from it (its checkedAt), after that run again; an
  // unknown answer is not kept.
  const t0 = RealDate.UTC(2026, 10, 14, 13, 0, 0, 0);
  await scenario('cache', [
    { at: t0, requests: [base()], hf: hf1(ggufAnswer(MTP_GGUF)) },
    { at: t0 + 299999, requests: [base(), base({ role: 'admin' })], hf: {} },
    { at: t0 + 300000, requests: [base()], hf: hf1(ggufAnswer(PLAIN_GGUF)) },
    { at: t0 + 300001, requests: [base()], hf: {} },
    { at: t0 + 400000, requests: [base({ query: q('Q8_0'), ...lem([ROW('Q8_0', ['qa-x.gguf'])]) })], hf: { [metaUrl(repo)]: { status: 500 } } },
    { at: t0 + 400001, requests: [base({ query: q('Q8_0'), ...lem([ROW('Q8_0', ['qa-x.gguf'])]) })], hf: { [metaUrl(repo)]: META(), [fileUrl(repo, REV, 'qa-x.gguf')]: ggufAnswer(DECLARED_GGUF) } },
    { at: t0 + 400002, requests: [base({ query: q('Q8_0'), ...lem([ROW('Q8_0', ['qa-x.gguf'])]) })], hf: {} },
  ]);
  // 100 entries: the oldest is dropped; refreshing an expired entry drops the oldest too and keeps its place.
  const many = (i) => ({ query: `repo=qa-org/qa-${i}&variant=Q4_K_M`, script: lemonadeVariants([ROW('Q4_K_M', files1)]) });
  const manyHf = (i, g = PLAIN_GGUF) => happyHf(`qa-org/qa-${i}`, files1, [ggufAnswer(g)]);
  const steps = [];
  for (let i = 0; i < 100; i += 1) steps.push({ at: t0 + i, requests: [many(i)], hf: manyHf(i) });
  steps.push({ at: t0 + 200, requests: [many(0), many(99)], hf: {} });
  steps.push({ at: t0 + 300050, requests: [many(10)], hf: manyHf(10, MTP_GGUF) });
  steps.push({ at: t0 + 300051, requests: [many(0)], hf: manyHf(0, MTP_GGUF) });
  steps.push({ at: t0 + 300052, requests: [many(1)], hf: manyHf(1, MTP_GGUF) });
  steps.push({ at: t0 + 300053, requests: [many(10)], hf: {} });
  steps.push({ at: t0 + 300054, requests: [many(100)], hf: manyHf(100) });
  steps.push({ at: t0 + 300055, requests: [many(2)], hf: manyHf(2, MTP_GGUF) });
  await scenario('cache bound', steps);
  // At once: a second request for the same check shares its answer; a third distinct check is refused while two run.
  const at3 = t0 + 1000;
  await scenario('running', [
    { at: at3, requests: [many(1), many(1), many(2), many(3), many(2)], hf: { ...manyHf(1, MTP_GGUF), ...manyHf(2) } },
    { at: at3 + 1, requests: [many(3), many(1)], hf: manyHf(3) },
    { at: at3 + 2, requests: [many(4), many(5), base({ query: 'repo=qa' }), many(6)], hf: { ...manyHf(4), [metaUrl('qa-org/qa-5')]: { throw: 'timeout' } } },
    { at: at3 + 3, requests: [many(5), many(6)], hf: { ...manyHf(5), ...manyHf(6) } },
  ]);

  // PRNG scenarios: random selections, revision replies, prefixes.
  for (let n = 0; n < 150; n += 1) {
    const count = 1 + int(3);
    const files = Array.from({ length: count }, (_, i) => `qa-${n}-${i}${pick(['', '', '-é', ' x'])}.gguf`);
    const answers = files.map(() => {
      const r = rand();
      if (r < 0.1) return pick([{ status: 404 }, { throw: 'network' }, { throw: 'timeout' }, { status: 204 }]);
      const bytes = r < 0.4 ? pick([MTP_GGUF, PLAIN_GGUF, DECLARED_GGUF, SPLIT_GGUF(count), SPLIT_GGUF(count + 1)]) : randomGguf();
      return rand() < 0.2 ? { status: 200, parts: toParts(bytes), chunk: 1 + int(64) } : ggufAnswer(bytes, { chunk: 1 + int(128) });
    });
    const meta = rand() < 0.9 ? META() : pick([{ status: 404 }, META('x'), { status: 200, text: '' }, { throw: 'network' }]);
    await one(`random ${n}`, { query: q(), script: lemonadeVariants([ROW('Q4_K_M', files)]) }, { ...happyHf(repo, files, answers), [metaUrl(repo)]: meta }, t0 + 10_000_000 + n);
  }
}

(async () => {
  inspectEdges();
  for (let n = 0; n < 500; n += 1) addInspect(randomGguf());
  await pulls();
  await mtps();
  fs.rmSync(tmp, { recursive: true, force: true });
  if (problems.length) throw new Error(`the stubs saw what they did not expect:\n${[...new Set(problems)].join('\n')}`);

  // ── Output ────────────────────────────────────────────────────────────────────────────────
  const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
  const differences = ['body-unsupported', 'meta-not-json', 'meta-unsupported', 'meta-error-at-cap'];
  const text = `{"version":1,"generator":"crates/model-pull/tests/record_model_pull.cjs",\n"node":${JSON.stringify(process.version)},\n`
    + `"sources":${JSON.stringify(sources)},\n"blocks":${JSON.stringify(blocks)},\n"limit":${LIMIT},\n`
    + `"inspect":[\n${lines(inspectCases)}\n],\n"names":[\n${lines(names)}\n],\n"quants":[\n${lines(quants)}\n],\n`
    + `"pull":[\n${lines(pullCases)}\n],\n"mtp":[\n${lines(mtpScenarios)}\n],\n"differences":${JSON.stringify(differences)}}\n`;
  const requests = mtpScenarios.flatMap((s) => s.steps.flatMap((st) => st.requests));
  const counts = `${inspectCases.length} inspect (${inspectCases.filter((c) => c.out).length} read), ${names.length} names, ${quants.length} quants, `
    + `${pullCases.length} pull (${pullCases.filter((c) => c.status === 200).length} accepted), ${mtpScenarios.length} mtp scenarios with ${requests.length} requests `
    + `(${requests.filter((r) => r.body?.status === 'present').length} present, ${requests.filter((r) => r.body?.status === 'absent').length} absent, `
    + `${requests.filter((r) => r.body?.status === 'unknown').length} unknown)`;
  const at = process.argv.indexOf('--check');
  const finish = () => process.exit(0);
  if (at > 0) {
    const committed = fs.readFileSync(process.argv[at + 1], 'utf8');
    if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
    process.stderr.write(`ok: ${counts} recorded as committed\n`);
    finish();
  } else {
    process.stderr.write(`recorded: ${counts}\n`);
    process.stdout.write(text, finish);
  }
})().catch((e) => { process.stderr.write(`${e?.stack || e}\n`); process.exit(1); });
