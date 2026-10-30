'use strict';
// Records Core's own answers for GET /api/models/evidence and POST /api/models/evidence/import (the real
// routes/models.cjs, llamacpp-manager.cjs, evidence.cjs and model-evidence-import.cjs of the Core at CORE,
// before the routes moved) over synthetic models, a scripted router and a scripted model-card source.
// tests/model_evidence.rs replays the same operations through the Rust front and compares every answer and the
// evidence log it leaves (ids and clock times normalised). Synthetic data only; no engine is touched.
//
//   CORE=/path/to/core node record-evidence.cjs > evidence-cases.json
console.log = (...a) => console.error(...a); // Core logs to stdout; the output here is the JSON only
const http = require('node:http');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { Readable } = require('node:stream');
const CORE = process.env.CORE;
if (!CORE) throw new Error('CORE is required');
const server = (f) => require(path.join(CORE, 'server', f));
const { createLlamaCppManager } = server('llamacpp-manager.cjs');
const { createModelRoutes } = server('routes/models.cjs');
const { fetchJson: realFetchJson } = server('http.cjs');

// The router's address is part of every identity (a hash of it), so the recording and the replay use the same one.
const PORT = 18517;
const MTIME = 1_700_000_000; // seconds; fixed so the artifact fingerprint is the same in both runs
const SHA = 'a'.repeat(40);
const INI = (ctx = '8192', extra = '') => `version = 1\n\n[fx]\nmodel = /models/fx/fx.gguf\nctx-size = ${ctx}\n${extra}`;

const card = (over = {}) => ({
  id: 'acme/model-7b', sha: SHA, license: 'apache-2.0', pipeline_tag: 'text-generation', library_name: 'gguf',
  tags: ['gguf', 'chat', '<b>bold</b>', 'x'.repeat(90)],
  cardData: { license: 'apache-2.0', model_summary: 'A  general-purpose\n chat <i>model</i>.', 'model-index': [{ results: [{ task: { type: 'text-generation' }, dataset: { name: 'MMLU' }, metrics: [{ type: 'accuracy', value: 71.2 }, { name: 'f1', value: 'high' }] }] }] },
  ...over,
});
const GEN = { temperature: 0.7, top_p: 0.9, top_k: 40, min_p: 0.05, repetition_penalty: 1.1, do_sample: true };
const hf = (...pairs) => Object.fromEntries(pairs);
const CARD = '/api/models/acme/model-7b';
const GENCFG = `/api/resolve-cache/models/acme/model-7b/${SHA}/generation_config.json`;
const rec = (category, result, value, extra = {}) => ({ category, result, value, suite: { name: 'synthetic', version: 1 }, source: 'test', limitations: [], ...extra });

const S = [];
const add = (name, c) => S.push({ name, model: 'fx', role: 'admin', kind: 'llamacpp', ini: INI(), mmproj: false, hf: {}, ops: [{ op: 'get' }], ...c });
const get = { op: 'get' };

add('nothing recorded for a model whose file is visible', {});
add('a member may read the evidence', { role: 'member' });
add('recorded results are verified against the live identity', { ops: [
  { op: 'record', record: rec('context_capacity', 'passed', { ctx: 8192, appliedCtx: 8192, slots: 1 }, { source: 'calibration', limitations: ['prompt budget 120 s'] }) },
  { op: 'record', record: rec('vision', 'passed', null, { limitations: ['1×1 image accepted; not an accuracy test', 'A new sentence nobody translated'] }) },
  { op: 'record', record: rec('vision', 'failed', null, { limitations: ['The inference server needs a multimodal projector (mmproj) configured.'] }) },
  { op: 'record', record: rec('throughput', 'reported', { rate: 41.2 }, { limitations: ['median of 5 warm, uncontended requests; depends on prompt mix and max tokens'] }) },
  { op: 'record', record: rec('mtp_acceptance', 'reported', { rate: 0.62, drafted: 100, accepted: 62 }, { limitations: ['single reply; depends on content'] }) },
  get] });
add('the same result twice is one record; a rate within 0.05 is not a new one', { ops: [
  { op: 'record', record: rec('context_capacity', 'passed', { ctx: 4096 }) }, { op: 'record', record: rec('context_capacity', 'passed', { ctx: 4096 }) },
  { op: 'record', record: rec('mtp_acceptance', 'reported', { rate: 0.5 }) }, { op: 'record', record: rec('mtp_acceptance', 'reported', { rate: 0.52 }) },
  { op: 'record', record: rec('mtp_acceptance', 'reported', { rate: 0.7 }) }, get] });
add('a changed preset or file makes it stale; restoring it verifies again; a missing file is unavailable', { ops: [
  { op: 'record', record: rec('context_capacity', 'passed', { ctx: 8192 }) }, get,
  { op: 'ini', text: INI('16384') }, get, { op: 'ini', text: INI('8192') }, get,
  { op: 'file', fill: 9, size: 4096 }, get, { op: 'rmfile' }, get] });
// #2210: speculative decoding with a draft model. The hash must be the same on both sides, as must what is filed under it.
add('a speculative draft model in the preset', { draft: true, ini: 'version = 1\n\n[fx]\nmodel = /models/fx/fx.gguf\nctx-size = 8192\nspec-type = draft-simple\nspec-draft-model = /models/fx/draft.gguf\n', ops: [get,
  { op: 'record', record: rec('context_capacity', 'passed', { ctx: 8192 }) }, get] });
add('a vision projector and cache settings are part of the identity', { mmproj: true, ini: INI('8192', 'mmproj = /models/fx/mm.gguf\ncache-type-k = q8_0\nparallel = 2\nspec-type = draft-mtp\n'), ops: [
  { op: 'record', record: rec('vision', 'passed', null) }, get] });
add('a model that is not in the router listing but has a preset', { listing: 'empty', ops: [get] });
add('the router listing is down: the files come from the preset', { listing: 'down', ops: [{ op: 'record', record: rec('context_capacity', 'passed', { ctx: 1 }) }, get] });
add('the model-card import: card and sampling config', { model: 'acme/model-7b', ini: INI().replace('[fx]', '[acme/model-7b]'), hf: hf([CARD, { status: 200, body: card() }], [GENCFG, { status: 200, body: GEN }]), ops: [{ op: 'import', body: { model: 'acme/model-7b' } }, get] });
add('importing again changes nothing; a changed card is a new record; a changed config too', { model: 'acme/model-7b', ini: INI().replace('[fx]', '[acme/model-7b]'), hf: hf([CARD, { status: 200, body: card() }], [GENCFG, { status: 200, body: GEN }]), ops: [
  { op: 'import', body: { model: 'acme/model-7b' } }, { op: 'import', body: { model: 'acme/model-7b', checkpoint: 'acme/model-7b:Q4_K_M' } },
  { op: 'hf', set: { [CARD]: { status: 200, body: card({ sha: 'b'.repeat(40) }) } } }, { op: 'import', body: { model: 'acme/model-7b' } },
  { op: 'hf', set: { [CARD]: { status: 200, body: card({ license: 'mit', cardData: { license: 'mit' } }) } } }, { op: 'import', body: { model: 'acme/model-7b' } },
  { op: 'hf', set: { [GENCFG]: { status: 200, body: { ...GEN, temperature: 0.2 } } } }, { op: 'import', body: { model: 'acme/model-7b' } }, get] });
add('a definite removal of the sampling config retires it; a failure keeps it', { model: 'acme/model-7b', ini: INI().replace('[fx]', '[acme/model-7b]'), hf: hf([CARD, { status: 200, body: card() }], [GENCFG, { status: 200, body: GEN }]), ops: [
  { op: 'import', body: { model: 'acme/model-7b' } },
  { op: 'hf', set: { [GENCFG]: { status: 503, body: { error: 'down' } } } }, { op: 'import', body: { model: 'acme/model-7b' } },
  { op: 'hf', set: { [GENCFG]: { status: 404, body: { error: 'gone' } } } }, { op: 'import', body: { model: 'acme/model-7b' } },
  { op: 'import', body: { model: 'acme/model-7b' } }, get] });
add('a config that is not a sampling config retires the old values', { model: 'acme/model-7b', ini: INI().replace('[fx]', '[acme/model-7b]'), hf: hf([CARD, { status: 200, body: card() }], [GENCFG, { status: 200, body: GEN }]), ops: [
  { op: 'import', body: { model: 'acme/model-7b' } },
  { op: 'hf', set: { [GENCFG]: { status: 200, body: { do_sample: false, temperature: 0.1 } } } }, { op: 'import', body: { model: 'acme/model-7b' } }, get] });
add('a card without a full commit has no sampling source; a private or broken card is nothing', { model: 'acme/model-7b', ini: INI().replace('[fx]', '[acme/model-7b]'), hf: hf([CARD, { status: 200, body: card({ sha: 'abc123' }) }]), ops: [
  { op: 'import', body: { model: 'acme/model-7b' } }, get,
  { op: 'hf', set: { [CARD]: { status: 200, body: { private: true } } } }, { op: 'import', body: { model: 'acme/model-7b' } },
  { op: 'hf', set: { [CARD]: { status: 500, body: 'oops' } } }, { op: 'import', body: { model: 'acme/model-7b' } },
  { op: 'hf', set: { [CARD]: { status: 200, body: [1, 2] } } }, { op: 'import', body: { model: 'acme/model-7b' } }, get] });
add('a checkpoint override must name the same repository', { model: 'acme/model-7b', ini: INI().replace('[fx]', '[acme/model-7b]'), hf: hf([CARD, { status: 200, body: card() }], ['/api/models/evil/other', { status: 200, body: card({ license: 'evil' }) }]), ops: [
  { op: 'import', body: { model: 'acme/model-7b', checkpoint: 'evil/other' } }, { op: 'import', body: { model: 'acme/model-7b', checkpoint: '../x' } },
  { op: 'import', body: { model: 'acme/model-7b', checkpoint: 'acme/model-7b' } }, get] });
add('a model whose name is not a repository imports nothing', { hf: hf([CARD, { status: 200, body: card() }]), ops: [{ op: 'import', body: { model: 'fx' } }, get] });
add('card text is stripped, capped and bounded', { model: 'acme/model-7b', ini: INI().replace('[fx]', '[acme/model-7b]'), hf: hf([CARD, { status: 200, body: card({ cardData: { license: 'l'.repeat(300), model_summary: `${'<p>'.repeat(5)}${'word '.repeat(400)}\u0001\u0007tail`, 'model-index': [1, null, { results: [null, { task: { name: 'qa' }, dataset: {}, metrics: [{ type: 'em', value: 3 }, null, { value: { nested: 1 } }, { type: 'x', value: 1 }, { type: 'y', value: 2 }] }, ...Array.from({ length: 12 }, (_, i) => ({ task: { type: `t${i}` }, metrics: [{ type: 'a', value: i }, { type: 'b', value: i }, { type: 'c', value: i }] }))] }, { results: [] }, { results: [{ metrics: [] }] }] } }) }]), ops: [{ op: 'import', body: { model: 'acme/model-7b' } }, get] });
add('model families set the sampling plan; a card source wins over a family', { model: 'Qwen3-8B', ini: INI().replace('[fx]', '[Qwen3-8B]'), ops: [get] });
add('gpt-oss quirks', { model: 'gpt-oss-20b', ini: INI().replace('[fx]', '[gpt-oss-20b]'), ops: [get] });
add('no model', { ops: [{ op: 'raw', method: 'GET', query: '' }] });
add('an empty model', { ops: [{ op: 'raw', method: 'GET', query: '?model=' }] });
add('a model of 200 UTF-16 units is allowed, 201 is not', { ops: [{ op: 'raw', method: 'GET', query: `?model=${'a'.repeat(200)}` }, { op: 'raw', method: 'GET', query: `?model=${'a'.repeat(201)}` }, { op: 'raw', method: 'GET', query: `?model=${'😀'.repeat(100)}` }, { op: 'raw', method: 'GET', query: `?model=${'😀'.repeat(101)}` }] });
add('the first model parameter wins and plus is a space', { ops: [{ op: 'raw', method: 'GET', query: '?model=fx&model=other' }, { op: 'raw', method: 'GET', query: '?model=f+x' }, { op: 'raw', method: 'GET', query: '?model=%66x' }] });
add('import: a member is refused, a body that is not JSON or has no model is a 400', { ops: [
  { op: 'raw', method: 'POST', path: '/api/models/evidence/import', role: 'member', body: '{"model":"fx"}' },
  { op: 'raw', method: 'POST', path: '/api/models/evidence/import', body: '{nope' },
  { op: 'raw', method: 'POST', path: '/api/models/evidence/import', body: '' },
  { op: 'raw', method: 'POST', path: '/api/models/evidence/import', body: '{"model":5}' },
  { op: 'raw', method: 'POST', path: '/api/models/evidence/import', body: '[1]' },
  { op: 'raw', method: 'POST', path: '/api/models/evidence/import', body: `{"model":"${'a'.repeat(201)}"}` },
  { op: 'raw', method: 'POST', path: '/api/models/evidence/import', body: '{"model":"fx","checkpoint":5}' }] });
add('an engine without evidence: GET and import are 404', { kind: 'none', ops: [{ op: 'raw', method: 'GET', query: '?model=fx' }, { op: 'raw', method: 'POST', path: '/api/models/evidence/import', body: '{"model":"fx"}' }] });
add('an unreadable models.ini (a duplicate section) is the preset error', { ini: INI() + '\n[fx]\nmodel = /models/fx/x.gguf\n', ops: [get] });
add('a models.ini over the editor limit', { ini: null, iniPad: 1024 * 1024, ops: [get] });

async function runOne(c) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'evidence-rec-'));
  const modelsDir = path.join(dir, 'models');
  fs.mkdirSync(path.join(modelsDir, 'fx'), { recursive: true });
  const modelFile = path.join(modelsDir, 'fx', 'fx.gguf');
  const touch = (f) => fs.utimesSync(f, MTIME, MTIME);
  fs.writeFileSync(modelFile, Buffer.alloc(2048, 7)); touch(modelFile);
  if (c.mmproj) { fs.writeFileSync(path.join(modelsDir, 'fx', 'mm.gguf'), Buffer.alloc(1024, 3)); touch(path.join(modelsDir, 'fx', 'mm.gguf')); }
  const ini = path.join(dir, 'models.ini');
  fs.writeFileSync(ini, c.iniPad ? `[fx]\nmodel = /models/fx/fx.gguf\n; ${'x'.repeat(c.iniPad)}\n` : c.ini);
  const evidenceDir = path.join(dir, 'evidence-0123456789abcdef');
  let hfTable = { ...c.hf };
  const listingRow = () => ({ id: c.model, status: { value: 'unloaded', preset: 'p', args: ['--model', `/models/fx/fx.gguf`, ...(c.mmproj ? ['--mmproj', '/models/fx/mm.gguf'] : [])] } });
  const srv = http.createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      const send = (status, text) => { res.writeHead(status, { 'Content-Type': 'application/json' }); res.end(text); };
      if (req.url === '/models') {
        if (c.listing === 'down') { req.socket.destroy(); return; }
        return send(200, JSON.stringify({ data: c.listing === 'empty' ? [] : [listingRow()] }));
      }
      if (req.url === '/props') return send(200, JSON.stringify({ build_info: 'b-synthetic' }));
      return send(404, '{}');
    });
  });
  await new Promise((resolve, reject) => { srv.once('error', reject); srv.listen(PORT, '127.0.0.1', resolve); });
  const fetchJson = async (url, opts, timeout, max) => {
    const u = new URL(url);
    if (u.hostname === 'huggingface.co') {
      const hit = hfTable[u.pathname];
      if (!hit) return { ok: false, status: 404, body: { error: 'not found' } };
      return { ok: hit.status >= 200 && hit.status < 300, status: hit.status, body: hit.body };
    }
    return realFetchJson(url, opts, timeout, max);
  };
  const llama = c.kind === 'llamacpp';
  const manager = llama
    ? createLlamaCppManager({ baseUrl: `http://127.0.0.1:${PORT}/v1`, apiKey: 'local', fetchJson, presetPath: ini, evidenceDir, autoconfig: { modelsPath: modelsDir }, downloadStatePath: path.join(dir, 'dl.json'), reloadStatePath: path.join(dir, 'reload.json') })
    : { enabled: false, kind: 'none', capabilities: {} };
  const sent = [];
  const routes = createModelRoutes({
    json: (res, status, body) => { sent.push({ status, body }); },
    readBody: async (req) => { let s = ''; for await (const x of req) s += x; return s; },
    readJson: async (req) => { let s = ''; for await (const x of req) s += x; try { return s ? JSON.parse(s) : {}; } catch { throw Object.assign(new SyntaxError('invalid JSON'), { status: 400 }); } },
    fetchJson, env: {}, modelManager: manager,
    getProvider: () => ({}), providerHeaders: () => ({}), DEFAULT_PROVIDER_ID: 'default', createVisionProbe: () => async () => ({}), currentWorkspace: () => ({ userId: 'u' }),
    service: { modelScanCache: new Map(), refreshModelScan() {}, autoRoles: () => null, setAutoRoles() {}, ensureRolesLoaded() {}, servedCatalogue: async () => [], modelsInstalled: async () => [], deriveUserModelName: (x) => x, lastLoadedModel: () => null, clearLastLoadedModel() {}, clearRoleReferences: () => [] },
  });
  const call = async ({ method, url, role, body }) => {
    sent.length = 0;
    const req = Object.assign(Readable.from(body === undefined || body === '' ? [] : [Buffer.from(body)]), { method, headers: {}, url });
    const res = { once() {}, off() {}, setHeader() {}, writableEnded: false };
    const u = new URL('http://localhost' + url);
    let thrown = null;
    try { await routes(req, res, { path: u.pathname, authn: { user: { id: 'u1', role } }, url: u }); } catch (e) { thrown = e; }
    const out = sent.at(-1) || null;
    if (thrown && !out) {
      const status = Number(thrown.status);
      return { status: status >= 400 && status < 500 ? status : 500, body: JSON.stringify({ error: status >= 400 && status < 500 ? String(thrown.message || 'Request failed') : 'Internal error' }) };
    }
    return { status: out?.status ?? null, body: out ? JSON.stringify(out.body) : null };
  };
  const answers = [];
  for (const op of c.ops) {
    if (op.op === 'get') answers.push(await call({ method: 'GET', url: `/api/models/evidence?model=${encodeURIComponent(c.model)}`, role: c.role }));
    else if (op.op === 'raw') answers.push(await call({ method: op.method, url: (op.path || '/api/models/evidence') + (op.query || ''), role: op.role || c.role, body: op.body }));
    else if (op.op === 'record') await manager.recordEvidence(c.model, op.record);
    else if (op.op === 'import') answers.push(await call({ method: 'POST', url: '/api/models/evidence/import', role: 'admin', body: JSON.stringify(op.body) }));
    else if (op.op === 'ini') fs.writeFileSync(ini, op.text);
    else if (op.op === 'file') { fs.writeFileSync(modelFile, Buffer.alloc(op.size, op.fill)); touch(modelFile); }
    else if (op.op === 'rmfile') fs.rmSync(modelFile);
    else if (op.op === 'hf') hfTable = { ...hfTable, ...op.set };
  }
  srv.closeAllConnections?.();
  await new Promise((resolve) => srv.close(resolve));
  const log = fs.existsSync(path.join(evidenceDir, 'evidence.jsonl')) ? fs.readFileSync(path.join(evidenceDir, 'evidence.jsonl'), 'utf8') : null;
  fs.rmSync(dir, { recursive: true, force: true });
  return { answers, log };
}

(async () => {
  const cases = [];
  for (const c of S) cases.push({ ...c, expect: await runOne(c) });
  process.stdout.write(JSON.stringify({ core: process.env.CORE_REF || null, port: PORT, mtime: MTIME, cases }, null, 1) + '\n');
})().catch((e) => { console.error(e); process.exit(1); });
