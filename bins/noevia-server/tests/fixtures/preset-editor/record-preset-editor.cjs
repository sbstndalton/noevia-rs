'use strict';
// Records Core's own answers for PUT /api/models/preset (the real routes/models.cjs, llamacpp-manager.cjs
// applyPreset, llamacpp-presets.cjs, models-ini-writer.cjs and inference-maintenance.cjs of the Core at CORE,
// before the editor moved to the front) against a scripted router, inference gate and Model Loader on one port,
// for synthetic models.ini texts and synthetic GGUF headers. tests/preset_editor.rs replays the same scenarios
// through the Rust front and compares the status, the body text, the requests the router, the gate and the loader
// saw, every models.ini* file left behind and the reload guard state file. Synthetic data only; no engine is touched.
//
//   CORE=/path/to/core-at-3521cdc2 node record-preset-editor.cjs > preset-editor-cases.json
console.log = (...a) => console.error(...a); // Core logs to stdout; the output here is the JSON only
console.warn = (...a) => console.error(...a);
const http = require('node:http');
const fs = require('node:fs');
const os = require('node:os');
const crypto = require('node:crypto');
const path = require('node:path');
const { Readable } = require('node:stream');
const CORE = process.env.CORE;
if (!CORE) throw new Error('CORE is required');
const server = (f) => require(path.join(CORE, 'server', f));
const { createLlamaCppManager } = server('llamacpp-manager.cjs');
const { createModelRoutes } = server('routes/models.cjs');
const { fetchJson, errorResponse, readJson } = server('http.cjs');
const { createModelsIniWriter } = server('models-ini-writer.cjs');
const { engineView } = server('preset-reload.cjs');
const sha = (t) => crypto.createHash('sha256').update(t).digest('hex');

// A synthetic GGUF v3 header (no tensors), as in inference-budget.test.cjs.
function gguf(kv) {
  const u32 = n => { const b = Buffer.alloc(4); b.writeUInt32LE(n); return b; };
  const u64 = n => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b; };
  const str = s => { const b = Buffer.from(s); return Buffer.concat([u64(b.length), b]); };
  const val = (type, v) => type === 4 ? u32(v) : type === 8 ? str(v) : (() => { throw Error('type'); })();
  return Buffer.concat([Buffer.from('GGUF'), u32(3), u64(0), u64(Object.keys(kv).length), ...Object.entries(kv).map(([k, [t, v]]) => Buffer.concat([str(k), u32(t), val(t, v)]))]);
}
const HYBRID_HEADER = { 'general.architecture': [8, 'qwen35'], 'qwen35.context_length': [4, 262144], 'qwen35.embedding_length': [4, 4096], 'qwen35.block_count': [4, 32], 'qwen35.attention.head_count': [4, 16], 'qwen35.attention.head_count_kv': [4, 4], 'qwen35.attention.key_length': [4, 256], 'qwen35.attention.value_length': [4, 256], 'qwen35.full_attention_interval': [4, 4], 'qwen35.ssm.state_size': [4, 128], 'tokenizer.chat_template': [8, '{{ messages }}'] };

const INI = '[*]\ncache-ram = 512\n\n[m]\nmodel = /models/q.gguf\nctx-size = 4096\n\n[other]\nmodel = /models/o.gguf\nparallel = 2\n';
const row = (id, status = 'unloaded', args = ['--model', '/models/q.gguf']) => ({ id, status: { value: status, preset: 'p-' + id, args } });
const rowsM = (status = 'unloaded') => [row('m', status), row('other', 'unloaded', ['--model', '/models/o.gguf'])];
const listing = (rows) => ({ status: 200, rows });
const BODY = (options, extra = {}) => JSON.stringify({ model: 'm', baseRevision: '$REV', confirmReload: true, options, ...extra });

const S = [];
const add = (name, c) => S.push({
  name, ini: INI, kind: 'llamacpp', presetPath: true, mount: false, gguf: false, budget: null, writer: 'web', gate: 'grant', gateToken: true,
  router: { listings: [listing(rowsM())] }, request: { role: 'admin', body: BODY({ 'ctx-size': '8192' }) }, state: null, ...c,
});

// ── the gates and the request ────────────────────────────────────────────────────────────────
add('a member is refused', { request: { role: 'member', body: BODY({ 'ctx-size': '8192' }) } });
add('an engine without a preset store is 404', { kind: 'none' });
add('no preset file is 501', { presetPath: false });
add('invalid JSON', { request: { role: 'admin', body: '{nope' } });
add('no body at all asks for the confirmation', { request: { role: 'admin', body: '' } });
add('a null body asks for the confirmation', { request: { role: 'admin', body: 'null' } });
add('an array body asks for the confirmation', { request: { role: 'admin', body: '[1]' } });
add('confirmReload missing', { request: { role: 'admin', body: JSON.stringify({ model: 'm', baseRevision: '$REV', options: { 'ctx-size': '8192' } }) } });
add('confirmReload as a string is not true', { request: { role: 'admin', body: BODY({ 'ctx-size': '8192' }, { confirmReload: 'true' }) } });
// Escaped lone UTF-16 units remain JSON strings; unused fields are ignored by Core.
add('lone surrogate in an ignored field', { request: { role: 'admin', body: BODY({ 'ctx-size': '8192' }, { ignored: '\ud800' }) } });
add('low lone surrogate in an ignored field', { request: { role: 'admin', body: BODY({ 'ctx-size': '8192' }, { ignored: '\udfff' }) } });
add('lone surrogate in an invalid option value', { request: { role: 'admin', body: BODY({ 'ctx-size': '\ud800' }) } });
add('lone surrogate in an invalid model string', { request: { role: 'admin', body: BODY({ 'ctx-size': '8192' }, { model: '\ud800' }) } });
// ── the apply that works ─────────────────────────────────────────────────────────────────────
add('one option changes and the router reloads', {});
add('the router reloaded: its listing after it is read', { router: { listings: [listing(rowsM()), listing(rowsM()), listing(rowsM())] } });
add('several options, one cleared', { request: { role: 'admin', body: BODY({ 'ctx-size': '16384', parallel: '3', 'flash-attn': 'on', 'n-gpu-layers': '' }) } });
add('a model with no section gets one', { router: { listings: [listing([row('fresh', 'unloaded'), ...rowsM()])] }, request: { role: 'admin', body: JSON.stringify({ model: 'fresh', baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '2048', temp: '0.7' } }) } });
add('a cache-ram above the hard maximum is brought down', { request: { role: 'admin', body: BODY({ 'cache-ram': '999999' }) } });
add('a negative cache-ram is brought to the maximum', { request: { role: 'admin', body: BODY({ 'cache-ram': '-1' }) } });
add('a zero cache-ram stays', { request: { role: 'admin', body: BODY({ 'cache-ram': '0' }) } });
add('an unset cache-ram inherits the global one (kept)', { ini: '[*]\ncache-ram = 256\n\n[m]\nmodel = /models/q.gguf\n', request: { role: 'admin', body: BODY({ 'ctx-size': '8192' }) } });
add('no cache-ram anywhere gets the cap', { ini: '[m]\nmodel = /models/q.gguf\n', request: { role: 'admin', body: BODY({ 'ctx-size': '8192' }) } });
add('an inherited cache-ram above the maximum is brought down', { ini: '[*]\ncache-ram = 9999\n\n[m]\nmodel = /models/q.gguf\n', request: { role: 'admin', body: BODY({ 'ctx-size': '8192' }) } });
add('an embedding model has no prompt cache', { ini: '[*]\ncache-ram = 512\n\n[embed-m]\nmodel = /models/e.gguf\n', router: { listings: [listing([row('embed-m')])] }, request: { role: 'admin', body: JSON.stringify({ model: 'embed-m', baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '2048' } }) } });
add('a section flagged as reranking has no prompt cache', { ini: '[r]\nmodel = /models/r.gguf\n--reranking = on\n', router: { listings: [listing([row('r')])] }, request: { role: 'admin', body: JSON.stringify({ model: 'r', baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '2048' } }) } });
add('an alias in the file counts as the section value', { ini: '[m]\nmodel = /models/q.gguf\nLLAMA_ARG_CACHE_RAM = 8192\nc = 1024\n', request: { role: 'admin', body: BODY({ 'ctx-size': '8192' }) } });
add('a file with CRLF lines and trailing newlines', { ini: '[*]\r\ncache-ram = 512\r\n\r\n[m]\r\nmodel = /models/q.gguf\r\nctx-size = 4096\r\n\r\n\r\n', request: { role: 'admin', body: BODY({ 'ctx-size': '8192' }) } });
add('decimal sampling options', { request: { role: 'admin', body: BODY({ temp: '0.8', 'top-p': '0.95', 'repeat-penalty': '1.1' }) } });
// ── validation ───────────────────────────────────────────────────────────────────────────────
add('an unknown option', { request: { role: 'admin', body: BODY({ nope: '1' }) } });
add('an alias is not an option name', { request: { role: 'admin', body: BODY({ c: '4096' }) } });
add('a number is not a value', { request: { role: 'admin', body: BODY({ 'ctx-size': 4096 }) } });
add('an invalid value', { request: { role: 'admin', body: BODY({ 'ctx-size': 'lots' }) } });
add('an invalid cache-ram value', { request: { role: 'admin', body: BODY({ 'cache-ram': 'big' }) } });
add('options missing', { request: { role: 'admin', body: JSON.stringify({ model: 'm', baseRevision: '$REV', confirmReload: true }) } });
add('options is an array', { request: { role: 'admin', body: JSON.stringify({ model: 'm', baseRevision: '$REV', confirmReload: true, options: [] }) } });
add('options is empty', { request: { role: 'admin', body: BODY({}) } });
add('the first of two invalid options is named', { request: { role: 'admin', body: BODY({ zzz: '1', 'ctx-size': 'x' }) } });
add('a model name that is not valid', { router: { listings: [listing([row('a b')])] }, request: { role: 'admin', body: JSON.stringify({ model: 'a b', baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '2048' } }) } });
add('the model is not a string', { request: { role: 'admin', body: JSON.stringify({ model: 7, baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '2048' } }) } });
add('a stale base revision', { request: { role: 'admin', body: BODY({ 'ctx-size': '8192' }, { baseRevision: 'f'.repeat(64) }) } });
add('no base revision', { request: { role: 'admin', body: JSON.stringify({ model: 'm', confirmReload: true, options: { 'ctx-size': '8192' } }) } });
add('the micro batch is larger than the batch', { request: { role: 'admin', body: BODY({ 'batch-size': '256', 'ubatch-size': '512' }) } });
add('the micro batch is larger than the default batch', { request: { role: 'admin', body: BODY({ 'ubatch-size': '4096' }) } });
add('the micro batch against the global batch size', { ini: '[*]\nbatch-size = 128\n\n[m]\nmodel = /models/q.gguf\n', request: { role: 'admin', body: BODY({ 'ubatch-size': '512' }) } });
add('a duplicate section in models.ini', { ini: INI + '\n[m]\nmodel = /models/q2.gguf\n' });
// ── the model and the engine ─────────────────────────────────────────────────────────────────
add('a model the router does not list', { request: { role: 'admin', body: JSON.stringify({ model: 'ghost', baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '2048' } }) } });
add('a model that is loaded refuses', { router: { listings: [listing(rowsM('loaded'))] } });
add('another model that is loading refuses', { router: { listings: [listing([row('m'), row('other', 'loading')])] } });
add('a sleeping model refuses', { router: { listings: [listing(rowsM('sleeping'))] } });
add('a system routing model by name', { ini: '[laya_x]\nmodel = /models/l.gguf\n', router: { listings: [listing([row('laya_x')])] }, request: { role: 'admin', body: JSON.stringify({ model: 'laya_x', baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '2048' } }) } });
add('a system routing model by path', { ini: '[x]\nmodel = /models/laya-detector.gguf\n', router: { listings: [listing([row('x', 'unloaded', ['--model', '/models/laya-detector.gguf'])])] }, request: { role: 'admin', body: JSON.stringify({ model: 'x', baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '2048' } }) } });
add('a system routing model by the roles file', { roles: '{"m":"system-routing"}' });
add('a roles file role that is not system routing wins over the name', { ini: '[laya_y]\nmodel = /models/l.gguf\n', roles: '{"laya_y":"chat"}', router: { listings: [listing([row('laya_y')])] }, request: { role: 'admin', body: JSON.stringify({ model: 'laya_y', baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '2048' } }) } });
add('the router listing fails: its answer is passed on', { router: { listings: [{ status: 503, text: '{"error":"router down"}' }] } });
add('the router listing is plain text', { router: { listings: [{ status: 502, text: 'Bad Gateway' }] } });
add('the router listing is ok but has no data array', { router: { listings: [{ status: 200, text: '{"models":[]}' }] } });
add('the router listing data is not an array', { router: { listings: [{ status: 200, text: '{"data":{"a":1}}' }] } });
add('the router listing has a null row', { router: { listings: [{ status: 200, text: '{"data":[null]}' }] } });
add('the router does not answer the listing', { router: { listings: [{ drop: true }] } });
add('the second listing fails: its answer is passed on', { router: { listings: [listing(rowsM()), { status: 500, text: '{"error":"x"}' }] } });
add('the second listing does not answer', { router: { listings: [listing(rowsM()), { drop: true }] } });
add('the second listing is not a listing', { router: { listings: [listing(rowsM()), { status: 200, text: '{"data":"x"}' }] } });
// ── the memory budget ────────────────────────────────────────────────────────────────────────
const sized = { mount: true, gguf: true, ini: '[*]\ncache-ram = 0\n\n[m]\nmodel = /models/q.gguf\nctx-size = 4096\n\n[other]\nmodel = /models/o.gguf\n' };
add('a save that fits the budget', { ...sized, budget: 64 });
add('a save above the budget is refused before anything is written', { ...sized, budget: 3, request: { role: 'admin', body: BODY({ 'ctx-size': '262144', 'cache-type-k': 'f16', 'cache-type-v': 'f16' }) } });
add('a save above the budget on the first listing too', { ...sized, budget: 3, router: { listings: [listing(rowsM()), listing(rowsM()), listing(rowsM())] }, request: { role: 'admin', body: BODY({ 'ctx-size': '262144', 'cache-type-k': 'f16', 'cache-type-v': 'f16' }) } });
add('an unset prompt cache is sized as the cap unless it is free', { ...sized, budget: 1, ini: '[m]\nmodel = /models/q.gguf\n', request: { role: 'admin', body: BODY({ 'ctx-size': '8192' }) } });
add('an embedding model is sized with no prompt cache', { ...sized, budget: 64, ini: '[embed-m]\nmodel = /models/q.gguf\n', router: { listings: [listing([row('embed-m')])] }, request: { role: 'admin', body: JSON.stringify({ model: 'embed-m', baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '2048' } }) } });
add('a file context that is negative refuses as an invalid size', { ...sized, budget: 64, ini: '[m]\nmodel = /models/q.gguf\nctx-size = -1\ncache-ram = 0\n', request: { role: 'admin', body: BODY({ parallel: '2' }) } });
add('a file context that is not a whole number refuses as an invalid size', { ...sized, budget: 64, ini: '[m]\nmodel = /models/q.gguf\nctx-size = 1e3\ncache-ram = 0\n', request: { role: 'admin', body: BODY({ parallel: '2' }) } });
add('a file batch size of zero refuses as an invalid size', { ...sized, budget: 64, ini: '[m]\nmodel = /models/q.gguf\nbatch-size = 0\ncache-ram = 0\n', request: { role: 'admin', body: BODY({ parallel: '2' }) } });
add('a file context beyond the safe range refuses as an invalid size', { ...sized, budget: 64, ini: '[m]\nmodel = /models/q.gguf\nctx-size = 99999999999999999999\ncache-ram = 0\n', request: { role: 'admin', body: BODY({ parallel: '2' }) } });
add('an invalid size is refused without a budget too', { ini: '[m]\nmodel = /models/q.gguf\nctx-size = -1\n', request: { role: 'admin', body: BODY({ parallel: '2' }) } });
add('a context that is not a whole number', { ...sized, budget: 64, request: { role: 'admin', body: BODY({ 'ctx-size': '0x10' }) } });
add('a context of zero is the native context', { ...sized, budget: 64, request: { role: 'admin', body: BODY({ 'ctx-size': '0' }) } });
add('a batch size of zero', { ...sized, budget: 64, request: { role: 'admin', body: BODY({ 'batch-size': '0' }) } });
add('a micro batch larger than the batch is refused as invalid size', { ...sized, budget: 64, request: { role: 'admin', body: BODY({ 'batch-size': '64', 'ubatch-size': '128' }) } });
add('a plus sign before the context', { ...sized, budget: 64, request: { role: 'admin', body: BODY({ 'ctx-size': '+4096' }) } });
add('an unreadable model is no estimate', { ...sized, budget: 3, gguf: false, request: { role: 'admin', body: BODY({ 'ctx-size': '262144', 'cache-type-k': 'f16', 'cache-type-v': 'f16' }) } });
add('no models mount is no estimate', { ...sized, mount: false, budget: 3, request: { role: 'admin', body: BODY({ 'ctx-size': '262144', 'cache-type-k': 'f16', 'cache-type-v': 'f16' }) } });
add('no budget is no estimate', { ...sized, budget: null, request: { role: 'admin', body: BODY({ 'ctx-size': '262144', 'cache-type-k': 'f16', 'cache-type-v': 'f16' }) } });
add('a budget of zero is no estimate', { ...sized, budget: 0, request: { role: 'admin', body: BODY({ 'ctx-size': '262144', 'cache-type-k': 'f16', 'cache-type-v': 'f16' }) } });
add('a stale revision is not a dry run: the real check answers', { ...sized, budget: 3, request: { role: 'admin', body: BODY({ 'ctx-size': '262144', 'cache-type-k': 'f16', 'cache-type-v': 'f16' }, { baseRevision: 'e'.repeat(64) }) } });
// ── the maintenance hold ─────────────────────────────────────────────────────────────────────
add('the gate says the engine is busy', { gate: { refuse: { status: 409, code: 'engine_busy', message: 'The engine is busy.' } } });
add('another client holds the gate', { gate: { refuse: { status: 503, code: 'maintenance_held', message: 'Held.' } } });
add('the gate rejects the admin token', { gate: { refuse: { status: 403, code: 'admin_required', message: 'The maintenance hold needs the gate\'s admin token.' } } });
add('the gate does not answer', { gate: 'drop' });
add('no gate at the engine address: this process holds alone', { gate: 'absent' });
add('no admin token: this process holds alone', { gateToken: false });
// ── the commit through the Model Loader ──────────────────────────────────────────────────────
const L = (...calls) => ({ writer: 'loader', loader: calls });
add('the loader commits', L('ok'));
add('the loader confirms nothing and wrote nothing', L('drop'));
add('the loader wrote the file but the reply was lost', L('lost'));
add('the loader answered 500 and wrote the file', L('lost500'));
add('the loader rejects the token', L('401'));
add('the loader has no such endpoint', L('404'));
add('the loader sees another revision', L('409'));
add('the loader rejects the text', L('400'));
add('the loader reply is not JSON ok', L('weird'));
add('the loader wrote and someone else changed the file after', L('thirdparty'));
// ── the reload fails ─────────────────────────────────────────────────────────────────────────
add('the router refuses the reload: the file is put back', { router: { listings: [listing(rowsM())], reload: [{ status: 500, text: '{"error":"x"}' }] } });
add('the router drops the reload connection', { router: { listings: [listing(rowsM())], reload: [{ drop: true }] } });
add('the router refuses both reloads', { router: { listings: [listing(rowsM())], reload: [{ status: 500, text: 'no' }, { status: 500, text: 'no' }] } });
add('the first reload fails and the second works', { router: { listings: [listing(rowsM())], reload: [{ status: 502, text: 'no' }, { status: 200, text: '{"success":true}' }] } });
add('the file is changed by someone else during the reload', { router: { listings: [listing(rowsM())], reload: [{ status: 500, text: 'no' }], iniOnReload: INI + '; operator edit\n' } });
add('the loader cannot restore after a failed reload (nothing written)', { ...L('ok', 'drop'), router: { listings: [listing(rowsM())], reload: [{ status: 500, text: 'no' }] } });
add('the loader cannot restore after a failed reload (rejects)', { ...L('ok', '401'), router: { listings: [listing(rowsM())], reload: [{ status: 500, text: 'no' }] } });
add('the loader restores after a failed reload', { ...L('ok', 'ok'), router: { listings: [listing(rowsM())], reload: [{ status: 500, text: 'no' }] } });
add('the loader restores after a failed reload but its reply is lost', { ...L('ok', 'lost'), router: { listings: [listing(rowsM())], reload: [{ status: 500, text: 'no' }] } });
add('a reload baseline from an earlier reload', { state: JSON.stringify({ text: INI, view: engineView(rowsM()) }) });
add('an unreadable reload baseline', { state: 'not json' });
// ── concurrency ──────────────────────────────────────────────────────────────────────────────
add('a second apply while the first holds the engine is refused', { concurrent: true, router: { listings: [listing(rowsM())], delay: { listing: 600 } } });
add('a second apply while the first reloads is refused', { concurrent: true, router: { listings: [listing(rowsM())], delay: { reload: 600 } } });

function seedFiles(dir, c) {
  const iniPath = path.join(dir, 'models.ini');
  fs.writeFileSync(iniPath, c.ini);
  if (c.gguf) fs.writeFileSync(path.join(dir, 'q.gguf'), gguf(HYBRID_HEADER));
  if (c.roles) fs.writeFileSync(path.join(dir, 'models.roles.json'), c.roles);
  return iniPath;
}

async function runOne(c) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'preset-rec-'));
  const iniPath = seedFiles(dir, c);
  const stateFile = path.join(dir, 'native-preset-reload-x.json');
  if (c.state !== null) fs.writeFileSync(stateFile, typeof c.state === 'string' ? c.state : JSON.stringify(c.state));
  const rev = sha(c.ini);
  const log = { router: [], gate: [], loader: [] };
  let listingCalls = 0, reloadCalls = 0, loaderCalls = 0;
  const r = c.router || {};
  const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms || 0));
  const srv = http.createServer((req, res) => {
    let body = '';
    req.on('data', (d) => { body += d; });
    req.on('end', async () => {
      const hold = req.headers['x-noevia-maintenance'] ? 'hold' : 'nohold';
      const url = req.url;
      const send = (status, text, ct = 'application/json') => { res.writeHead(status, { 'Content-Type': ct }); res.end(text); };
      if (url.startsWith('/gate/v1/')) {
        const norm = url.replace(/[0-9a-f]{32}/g, 'T');
        log.gate.push(`${req.method} ${norm}${req.method === 'POST' && norm === '/gate/v1/maintenance' ? ' ' + body : ''}`);
        if (c.gate === 'drop' && url === '/gate/v1/maintenance') { req.socket.destroy(); return; }
        if (url === '/gate/v1/maintenance' && req.method === 'POST') {
          if (c.gate === 'absent') return send(404, '{"error":"not found"}');
          if (c.gate && c.gate.refuse) return send(c.gate.refuse.status, JSON.stringify({ error: { type: 'gate_error', code: c.gate.refuse.code, message: c.gate.refuse.message, retryable: false } }));
          return send(201, JSON.stringify({ hold: '0123456789abcdef0123456789abcdef', idle_ttl_ms: 600000 }));
        }
        if (req.method === 'DELETE') { res.writeHead(204); return res.end(); }
        return send(200, '{}');
      }
      if (url === '/api/v1/models-ini' && req.method === 'PUT') {
        log.loader.push(`PUT ${url} ${body}`);
        const mode = (c.loader || [])[Math.min(loaderCalls++, (c.loader || ['ok']).length - 1)] || 'ok';
        const asked = JSON.parse(body);
        const current = fs.readFileSync(iniPath, 'utf8');
        const write = () => fs.writeFileSync(iniPath, asked.text);
        if (mode === 'ok') {
          if (sha(current) !== asked.baseRevision) return send(409, '{"ok":false}');
          write();
          return send(200, JSON.stringify({ ok: true, revision: sha(asked.text) }));
        }
        if (mode === 'lost') { write(); req.socket.destroy(); return; }
        if (mode === 'lost500') { write(); return send(500, '{"ok":false}'); }
        if (mode === 'drop') { req.socket.destroy(); return; }
        if (mode === 'thirdparty') { fs.writeFileSync(iniPath, asked.text + '; third party\n'); req.socket.destroy(); return; }
        if (mode === 'weird') return send(200, '{"ok":"maybe"}');
        if (mode === '400') return send(400, JSON.stringify({ detail: 'bad text '.repeat(40) }));
        return send(Number(mode), '{"ok":false}');
      }
      if (req.method === 'GET' && url === '/models') {
        log.router.push(`GET /models ${hold}`);
        const l = r.listings[Math.min(listingCalls++, r.listings.length - 1)];
        if (r.delay && r.delay.listing) await delay(r.delay.listing);
        if (l.drop) { req.socket.destroy(); return; }
        return send(l.status, l.text !== undefined ? l.text : JSON.stringify({ data: l.rows }));
      }
      if (req.method === 'GET' && url === '/models?reload=1') {
        log.router.push(`GET /models?reload=1 ${hold}`);
        const n = reloadCalls++;
        if (n === 0 && r.iniOnReload) fs.writeFileSync(iniPath, r.iniOnReload);
        if (r.delay && r.delay.reload) await delay(r.delay.reload);
        const list = r.reload || [];
        const rl = list[Math.min(n, list.length - 1)] || { status: 200, text: '{"success":true}' };
        if (rl.drop) { req.socket.destroy(); return; }
        return send(rl.status, rl.text);
      }
      if (req.method === 'POST' && url === '/models/unload') { log.router.push(`POST /models/unload ${hold} ${body}`); return send(200, '{"success":true}'); }
      log.router.push(`${req.method} ${url} ${hold}`);
      return send(404, '{}');
    });
  });
  await new Promise((resolve) => srv.listen(0, '127.0.0.1', resolve));
  const port = srv.address().port;
  const llama = c.kind === 'llamacpp';
  const writer = c.writer === 'loader' ? createModelsIniWriter({ mode: 'model-loader', url: `http://127.0.0.1:${port}`, token: 'synthetic-loader-token', fetchJson }) : null;
  const modelManager = llama
    ? createLlamaCppManager({
      baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 'local', fetchJson, ...(c.presetPath ? { presetPath: iniPath } : {}), reloadStatePath: stateFile,
      downloadStatePath: path.join(dir, 'dl.json'), gateAdminToken: c.gateToken ? 'synthetic-admin-token' : null, presetWriter: writer,
      autoconfig: { ...(c.mount ? { modelsPath: dir } : {}), cacheRam: { capMib: 1024, hardMaxMib: 2048 }, cacheRamMaxMib: 1024 },
      inferenceBudget: { budgetGib: () => c.budget },
    })
    : { enabled: false, kind: 'none', capabilities: {} };
  const routes = createModelRoutes({
    json: (res, status, body) => { res.sent.push({ status, body }); },
    readBody: async (req) => { let s = ''; for await (const x of req) s += x; return s; },
    readJson,
    fetchJson, env: {}, modelManager,
    getProvider: () => ({}), providerHeaders: () => ({}), DEFAULT_PROVIDER_ID: 'default', createVisionProbe: () => async () => ({}), currentWorkspace: () => ({ userId: 'u' }),
    service: { modelScanCache: new Map(), refreshModelScan() {}, autoRoles: () => null, setAutoRoles() {}, ensureRolesLoaded() {}, servedCatalogue: async () => [], modelsInstalled: async () => [], deriveUserModelName: (x) => x, lastLoadedModel: () => null, clearLastLoadedModel() {}, clearRoleReferences: () => [] },
  });
  const put = async (role, text) => {
    const req = Object.assign(Readable.from([Buffer.from(text)]), { method: 'PUT', headers: {}, url: '/api/models/preset' });
    const res = { once() {}, off() {}, setHeader() {}, writableEnded: false, sent: [] };
    const url = new URL('http://localhost/api/models/preset');
    try {
      await routes(req, res, { path: url.pathname, authn: { user: { id: 'u1', role } }, url });
    } catch (e) {
      const f = errorResponse(e);
      res.sent.push({ status: f.status, body: f.body });
    }
    const out = res.sent.at(-1) || null;
    return { status: out?.status ?? null, body: out ? JSON.stringify(out.body) : null };
  };
  const text = c.request.body.replaceAll('$REV', rev);
  let answers;
  if (c.concurrent) {
    const first = put(c.request.role, text);
    await delay(200);
    const second = put(c.request.role, text);
    answers = await Promise.all([first, second]);
  } else {
    answers = [await put(c.request.role, text)];
  }
  await delay(200);
  srv.closeAllConnections?.();
  await new Promise((resolve) => srv.close(resolve));
  const files = {};
  for (const name of fs.readdirSync(dir).sort()) if (name.startsWith('models.ini')) files[name] = fs.readFileSync(path.join(dir, name), 'utf8');
  const stateAfter = fs.existsSync(stateFile) ? fs.readFileSync(stateFile, 'utf8') : null;
  const result = { answers, router: log.router, gate: log.gate, loader: log.loader, files, state: stateAfter };
  fs.rmSync(dir, { recursive: true, force: true });
  return result;
}

(async () => {
  const cases = [];
  for (const c of S.filter(c => !process.env.CASE_FILTER || new RegExp(process.env.CASE_FILTER).test(c.name))) cases.push({ ...c, expect: await runOne(c) });
  process.stdout.write(JSON.stringify({ core: process.env.CORE_REF || null, cases }, null, 1) + '\n');
})().catch((e) => { console.error(e); process.exit(1); });
