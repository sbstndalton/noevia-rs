'use strict';
// Records Core's own answers for PUT /api/models/preset (the admin preset editor's save: the real
// routes/models.cjs and llamacpp-manager.cjs `applyPreset` of the Core at CORE, before the route
// moved) against a scripted router and inference gate on one port, for synthetic models.ini texts,
// model mounts and inference budgets. A request handler's thrown error is answered as index.cjs
// does (http.cjs answerUnhandled / errorResponse). tests/model_preset.rs replays the same scenarios
// through the Rust front and compares status, body text, the requests the router and the gate saw,
// the models.ini text, its recovery copies and the reload guard state file. Synthetic data only; no
// engine is touched.
//
//   CORE=/path/to/core node record-preset.cjs > preset-cases.json
console.log = (...a) => console.error(...a); // Core logs to stdout; the output here is the JSON only
console.warn = (...a) => console.error(...a);
const http = require('node:http');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { Readable } = require('node:stream');
const CORE = process.env.CORE;
if (!CORE) throw new Error('CORE is required');
const server = (f) => require(path.join(CORE, 'server', f));
const { createLlamaCppManager } = server('llamacpp-manager.cjs');
const { createModelRoutes } = server('routes/models.cjs');
const { fetchJson, readJson, errorResponse } = server('http.cjs');
const { engineView } = server('preset-reload.cjs');

const sha = (text) => crypto.createHash('sha256').update(text).digest('hex');
const INI = '[*]\ncache-ram = 512\n\n[A]\nmodel = /models/a.gguf\nctx-size = 4096\n\n[B]\nmodel = /models/b.gguf\n';
const row = (id, status, file = '/models/' + id + '.gguf') => ({ id, status: { value: status, preset: 'p-' + id, args: ['--model', file] } });
const rowsAB = (a = 'unloaded', b = 'unloaded') => [row('A', a), row('B', b)];
const listing = (rows) => ({ status: 200, rows });
const LISTED = { listings: [listing(rowsAB())] };
// The save the editor sends: `$REV` is the revision of the case's models.ini.
const save = (options, extra = {}) => JSON.stringify({ model: 'A', baseRevision: '$REV', confirmReload: true, options, ...extra });

// Synthetic GGUF v3 header (no tensors), as in core inference-budget.test.cjs.
function gguf(kv) {
  const u32 = (n) => { const b = Buffer.alloc(4); b.writeUInt32LE(n); return b; };
  const u64 = (n) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b; };
  const str = (s) => { const b = Buffer.from(s); return Buffer.concat([u64(b.length), b]); };
  const val = (type, v) => (type === 4 ? u32(v) : type === 8 ? str(v) : (() => { throw Error('type'); })());
  return Buffer.concat([Buffer.from('GGUF'), u32(3), u64(0), u64(Object.keys(kv).length), ...Object.entries(kv).map(([k, [t, v]]) => Buffer.concat([str(k), u32(t), val(t, v)]))]);
}
const QA_HEADER = { 'general.architecture': [8, 'qwen35'], 'qwen35.context_length': [4, 262144], 'qwen35.embedding_length': [4, 4096], 'qwen35.block_count': [4, 32], 'qwen35.attention.head_count': [4, 16], 'qwen35.attention.head_count_kv': [4, 4], 'qwen35.attention.key_length': [4, 256], 'qwen35.attention.value_length': [4, 256], 'qwen35.full_attention_interval': [4, 4], 'qwen35.ssm.state_size': [4, 128], 'tokenizer.chat_template': [8, '{{ messages }}'] };
const MOUNT = { 'q.gguf': gguf(QA_HEADER).toString('hex') };
const INI_Q = '[*]\ncache-ram = 512\n\n[Q]\nmodel = /models/q.gguf\nctx-size = 4096\ncache-ram = 0\n';
const Q_LISTED = { listings: [listing([row('Q', 'unloaded', '/models/q.gguf')])] };
const saveQ = (options) => JSON.stringify({ model: 'Q', baseRevision: '$REV', confirmReload: true, options });

const S = [];
const add = (name, c) => S.push({ name, ini: INI, kind: 'llamacpp', presets: true, gateToken: true, gate: 'grant', router: LISTED, request: { role: 'admin', body: save({ 'ctx-size': '8192' }) }, roles: null, mount: null, budget: null, state: null, ...c });

// Who may save, and what the route reads.
add('a member is refused', { request: { role: 'member', body: save({ 'ctx-size': '8192' }) } });
add('an engine without a model manager', { kind: 'none' });
add('a llama.cpp engine without LLAMACPP_PRESET_PATH', { presets: false });
add('invalid JSON', { request: { role: 'admin', body: '{nope' } });
add('a body over 1 MiB', { request: { role: 'admin', big: 1024 * 1024 + 1 } });
add('a body of exactly 1 MiB is read (and is not a save)', { request: { role: 'admin', big: 1024 * 1024 } });
add('no body at all', { request: { role: 'admin', body: '' } });
add('confirmReload missing', { request: { role: 'admin', body: JSON.stringify({ model: 'A', baseRevision: '$REV', options: { 'ctx-size': '8192' } }) } });
add('confirmReload false', { request: { role: 'admin', body: save({ 'ctx-size': '8192' }, { confirmReload: false }) } });
add('confirmReload "true" is not true', { request: { role: 'admin', body: save({ 'ctx-size': '8192' }, { confirmReload: 'true' }) } });
add('a null body', { request: { role: 'admin', body: 'null' } });
add('a JSON array body', { request: { role: 'admin', body: '[1]' } });
add('a JSON string body', { request: { role: 'admin', body: '"x"' } });

// The maintenance hold.
add('the gate says the engine is busy', { gate: { refuse: { status: 409, code: 'engine_busy', message: 'The engine is busy.' } } });
add('another client holds the gate', { gate: { refuse: { status: 503, code: 'maintenance_held', message: 'Held.' } } });
add('the gate rejects the admin token', { gate: { refuse: { status: 403, code: 'admin_required', message: 'The maintenance hold needs the gate\'s admin token.' } } });
add('the gate does not answer', { gate: 'drop' });
add('no gate at the engine address: this process holds alone', { gate: 'absent' });
add('no admin token: this process holds alone', { gateToken: false });

// The system routing model.
add('a system model by its name is refused', { ini: INI + '\n[laya-qa]\nmodel = /models/l.gguf\n', router: { listings: [listing([...rowsAB(), row('laya-qa', 'unloaded')])] }, request: { role: 'admin', body: JSON.stringify({ model: 'laya-qa', baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '8192' } }) } });
add('a system model by its role is refused', { roles: '{"A":"system-routing"}' });
add('a role that is not the system role keeps a laya name editable', { ini: INI + '\n[laya-qa]\nmodel = /models/l.gguf\n', roles: '{"laya-qa":"chat"}', router: { listings: [listing([...rowsAB(), row('laya-qa', 'unloaded')])] }, request: { role: 'admin', body: JSON.stringify({ model: 'laya-qa', baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '8192' } }) } });
add('a system model by its file path is refused', { router: { listings: [listing([row('A', 'unloaded', '/models/qa/Laya-router.gguf'), row('B', 'unloaded')])] } });
add('the system check skips a failed first listing', { router: { listings: [{ status: 503, text: '{"error":"router down"}' }, listing(rowsAB())] }, roles: '{"A":"system-routing"}' });

// Size knobs already in the section (the editor changes another option).
const knob = (line) => INI.replace('ctx-size = 4096', line);
add('ctx-size text in the file', { ini: knob('ctx-size = lots'), request: { role: 'admin', body: save({ temp: '0.7' }) } });
add('ctx-size negative in the file', { ini: knob('ctx-size = -1'), request: { role: 'admin', body: save({ temp: '0.7' }) } });
add('ctx-size 1e400 in the file', { ini: knob('ctx-size = 1e400'), request: { role: 'admin', body: save({ temp: '0.7' }) } });
add('ctx-size 0 in the file is the native context', { ini: knob('ctx-size = 0'), request: { role: 'admin', body: save({ temp: '0.7' }) } });
add('ctx-size +4096 in the file', { ini: knob('ctx-size = +4096'), request: { role: 'admin', body: save({ temp: '0.7' }) } });
add('ctx-size above the safe integers in the file', { ini: knob('ctx-size = 9007199254740993'), request: { role: 'admin', body: save({ temp: '0.7' }) } });
add('a long ctx-size text is cut at 40', { ini: knob('ctx-size = ' + 'x'.repeat(30) + 'é\u{1F600}' + 'y'.repeat(20)), request: { role: 'admin', body: save({ temp: '0.7' }) } });
add('ubatch-size zero in the file', { ini: knob('ubatch-size = 0'), request: { role: 'admin', body: save({ temp: '0.7' }) } });
add('batch-size negative in the file', { ini: knob('batch-size = -5'), request: { role: 'admin', body: save({ temp: '0.7' }) } });
add('batch-size text in the file', { ini: knob('batch-size = big'), request: { role: 'admin', body: save({ temp: '0.7' }) } });
add('ubatch-size above batch-size in the file', { ini: knob('ubatch-size = 4096\nbatch-size = 2048'), request: { role: 'admin', body: save({ temp: '0.7' }) } });
add('ubatch-size above the inherited batch-size', { ini: INI.replace('cache-ram = 512', 'cache-ram = 512\nbatch-size = 1024').replace('ctx-size = 4096', 'ubatch-size = 2048'), request: { role: 'admin', body: save({ temp: '0.7' }) } });
add('ubatch-size above the default batch', { ini: knob('ubatch-size = 4096'), request: { role: 'admin', body: save({ temp: '0.7' }) } });
// The same values sent by the editor.
add('the editor sends ctx-size -1', { request: { role: 'admin', body: save({ 'ctx-size': '-1' }) } });
add('the editor sends ubatch-size 0', { request: { role: 'admin', body: save({ 'ubatch-size': '0' }) } });
add('the editor sends ubatch above batch', { request: { role: 'admin', body: save({ 'ubatch-size': '4096', 'batch-size': '2048' }) } });
add('the editor sends ubatch above the inherited batch', { ini: INI.replace('cache-ram = 512', 'cache-ram = 512\nbatch-size = 1024'), request: { role: 'admin', body: save({ 'ubatch-size': '2048' }) } });

// The router.
add('the router listing fails: its answer is passed on', { router: { listings: [{ status: 503, text: '{"error":"router down"}' }] } });
add('the router listing is plain text', { router: { listings: [{ status: 502, text: 'Bad Gateway' }] } });
add('the router listing is ok but has no data array', { router: { listings: [{ status: 200, text: '{"models":[]}' }] } });
add('the router listing data is not an array', { router: { listings: [{ status: 200, text: '{"data":"x"}' }] } });
add('the router listing data is an object', { router: { listings: [{ status: 200, text: '{"data":{"id":"A"}}' }] } });
add('the router listing holds null before the model', { router: { listings: [{ status: 200, text: '{"data":[null,{"id":"A","status":{"value":"unloaded"}}]}' }] } });
add('the router listing holds null after the model', { router: { listings: [{ status: 200, text: '{"data":[{"id":"A","status":{"value":"unloaded"}},null]}' }] } });
add('the router listing holds a number', { router: { listings: [{ status: 200, text: '{"data":[{"id":"A","status":{"value":"unloaded"}},7]}' }] } });
add('the router drops the listing', { router: { listings: [{ drop: true }] } });
add('the second listing fails', { router: { listings: [listing(rowsAB()), { status: 500, text: '{"error":"x"}' }] } });
add('the model is not listed', { router: { listings: [listing([row('B', 'unloaded')])] } });
add('a model is loaded', { router: { listings: [listing(rowsAB('unloaded', 'loaded'))] } });
add('a model is sleeping', { router: { listings: [listing(rowsAB('sleeping', 'unloaded'))] } });
add('a row without a status', { router: { listings: [{ status: 200, text: '{"data":[{"id":"A","status":{"value":"unloaded"}},{"id":"B"}]}' }] } });

// The model, its options and the revision (presets.prepare).
add('an invalid option name', { request: { role: 'admin', body: save({ bogus: '1' }) } });
add('an invalid option value', { request: { role: 'admin', body: save({ 'ctx-size': 'abc' }) } });
add('an option value that is not a string', { request: { role: 'admin', body: save({ 'ctx-size': 8192 }) } });
add('an array-index option name sorts first', { request: { role: 'admin', body: save({ 'ctx-size': '8192', 0: 'x' }) } });
add('no options', { request: { role: 'admin', body: JSON.stringify({ model: 'A', baseRevision: '$REV', confirmReload: true }) } });
add('options as an array', { request: { role: 'admin', body: save(['8192']) } });
add('empty options', { request: { role: 'admin', body: save({}) } });
add('an invalid model name that the router lists', { ini: INI, router: { listings: [listing([...rowsAB(), row('qa model', 'unloaded')])] }, request: { role: 'admin', body: JSON.stringify({ model: 'qa model', baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '8192' } }) } });
add('a model that is not a string', { request: { role: 'admin', body: JSON.stringify({ model: 5, baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '8192' } }) } });
add('no model, and a row without an id', { router: { listings: [{ status: 200, text: '{"data":[{"status":{"value":"unloaded"}}]}' }] }, request: { role: 'admin', body: JSON.stringify({ baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '8192' } }) } });
add('a stale baseRevision', { request: { role: 'admin', body: save({ 'ctx-size': '8192' }, { baseRevision: '0'.repeat(64) }) } });
add('a baseRevision that is not a string', { request: { role: 'admin', body: save({ 'ctx-size': '8192' }, { baseRevision: 1 }) } });
add('models.ini with a duplicate section', { ini: INI + '\n[A]\nmodel = /models/a2.gguf\n' });

// Saved.
add('saved: the full answer', {});
add('saved under an alias, other options kept', { request: { role: 'admin', body: save({ c: '16384', temp: '0.6', 'top-k': '40' }) } });
add('saved: an option cleared', { request: { role: 'admin', body: save({ 'ctx-size': '' }) } });
add('saved: a model without a section gets one', { router: { listings: [listing([...rowsAB(), row('C', 'unloaded')])] }, request: { role: 'admin', body: JSON.stringify({ model: 'C', baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '8192' } }) } });
add('saved: an embedding model gets no prompt cache', { ini: INI + '\n[qa-embed]\nmodel = /models/e.gguf\n', router: { listings: [listing([...rowsAB(), row('qa-embed', 'unloaded')])] }, request: { role: 'admin', body: JSON.stringify({ model: 'qa-embed', baseRevision: '$REV', confirmReload: true, options: { 'ctx-size': '2048' } }) } });
add('saved: cache-ram above the hard maximum is clamped', { request: { role: 'admin', body: save({ 'cache-ram': '999999' }) } });
add('saved: extra body fields are ignored', { request: { role: 'admin', body: save({ 'ctx-size': '8192' }, { unload: true, backup: false }) } });
add('saved: the listing after the reload fails', { router: { listings: [listing(rowsAB()), listing(rowsAB()), { status: 500, text: '{"error":"x"}' }] } });
add('saved: the reload baseline was already recorded', { state: { text: INI, view: engineView(rowsAB()) } });

// The reload fails.
add('the router refuses the reload: the previous file is restored', { router: { ...LISTED, reload: { status: 500, text: '{"error":"x"}' } } });
add('the router drops the reload: the previous file is restored', { router: { ...LISTED, reload: { drop: true } } });
add('the reload fails and the file changed again: restore conflicts', { router: { ...LISTED, reload: { status: 500, text: '{"error":"x"}' }, iniAfterReload: INI + '; edited by an operator\n' } });
add('the reload is ok but the file then moved: the answer reads the moved file', { router: { ...LISTED, iniAfterReload: INI.replace('[B]', '[B]\ntemp = 0.5') } });

// The inference memory budget (a models mount, synthetic GGUF headers).
add('budget: the estimate fits', { ini: INI_Q, mount: MOUNT, budget: 64, router: Q_LISTED, request: { role: 'admin', body: saveQ({ 'ctx-size': '8192' }) } });
add('budget: over the budget', { ini: INI_Q, mount: MOUNT, budget: 3, router: Q_LISTED, request: { role: 'admin', body: saveQ({ 'ctx-size': '262144', 'cache-type-k': 'f16', 'cache-type-v': 'f16' }) } });
add('budget: a fractional budget', { ini: INI_Q, mount: MOUNT, budget: 2.5, router: Q_LISTED, request: { role: 'admin', body: saveQ({ 'ctx-size': '131072' }) } });
add('budget: the native context does not fit', { ini: INI_Q, mount: MOUNT, budget: 3, router: Q_LISTED, request: { role: 'admin', body: saveQ({ 'ctx-size': '0' }) } });
add('budget: an unbounded prompt cache', { ini: INI_Q, mount: MOUNT, budget: 64, router: Q_LISTED, request: { role: 'admin', body: saveQ({ 'cache-ram': '-1' }) } });
add('budget: the prompt cache from [*]', { ini: INI_Q.replace('cache-ram = 0\n', ''), mount: MOUNT, budget: 3, router: Q_LISTED, request: { role: 'admin', body: saveQ({ 'ctx-size': '65536' }) } });
add('budget: the model file is not on the mount', { ini: INI_Q.replace('/models/q.gguf', '/models/absent.gguf'), mount: MOUNT, budget: 3, router: { listings: [listing([row('Q', 'unloaded', '/models/absent.gguf')])] }, request: { role: 'admin', body: saveQ({ 'ctx-size': '262144' }) } });
add('budget: no mount means no estimate', { ini: INI_Q, mount: null, budget: 3, router: Q_LISTED, request: { role: 'admin', body: saveQ({ 'ctx-size': '262144', 'cache-type-k': 'f16', 'cache-type-v': 'f16' }) } });
add('budget: a size knob is refused before the estimate', { ini: INI_Q.replace('ctx-size = 4096', 'ctx-size = -2'), mount: MOUNT, budget: 3, router: Q_LISTED, request: { role: 'admin', body: saveQ({ temp: '0.7' }) } });

async function runOne(c) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'preset-rec-'));
  const iniPath = path.join(dir, 'models.ini');
  fs.writeFileSync(iniPath, c.ini);
  if (c.roles !== null) fs.writeFileSync(path.join(dir, 'models.roles.json'), c.roles);
  const models = path.join(dir, 'models');
  if (c.mount) {
    fs.mkdirSync(models);
    for (const [name, hex] of Object.entries(c.mount)) fs.writeFileSync(path.join(models, name), Buffer.from(hex, 'hex'));
  }
  const stateFile = path.join(dir, 'native-preset-reload-x.json');
  if (c.state !== null) fs.writeFileSync(stateFile, typeof c.state === 'string' ? c.state : JSON.stringify(c.state));
  const log = { router: [], gate: [] };
  let listingCalls = 0;
  const r = c.router || {};
  const srv = http.createServer((req, res) => {
    let body = '';
    req.on('data', (d) => { body += d; });
    req.on('end', () => {
      const hold = req.headers['x-noevia-maintenance'] ? 'hold' : 'nohold';
      const url = req.url;
      const send = (status, text, ct = 'application/json') => { res.writeHead(status, { 'Content-Type': ct }); res.end(text); };
      // The gate client's probe when the manager is made (its missing-token warning), not the save's.
      if (req.method === 'GET' && url === '/gate/v1/status') return send(200, '{}');
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
      if (req.method === 'GET' && url === '/models') {
        log.router.push(`GET /models ${hold}`);
        const i = Math.min(listingCalls++, r.listings.length - 1);
        const l = r.listings[i];
        if (l.drop) { req.socket.destroy(); return; }
        return send(l.status, l.text !== undefined ? l.text : JSON.stringify({ data: l.rows }));
      }
      if (req.method === 'GET' && url === '/models?reload=1') {
        log.router.push(`GET /models?reload=1 ${hold}`);
        if (r.iniAfterReload) fs.writeFileSync(iniPath, r.iniAfterReload);
        if (r.reload && r.reload.drop) { req.socket.destroy(); return; }
        const rl = r.reload || { status: 200, text: '{"success":true}' };
        return send(rl.status, rl.text);
      }
      log.router.push(`${req.method} ${url} ${hold}${req.method === 'POST' ? ' ' + body : ''}`);
      return send(404, '{}');
    });
  });
  await new Promise((resolve) => srv.listen(0, '127.0.0.1', resolve));
  const port = srv.address().port;
  const modelManager = c.kind === 'llamacpp'
    ? createLlamaCppManager({
      baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 'local', fetchJson,
      presetPath: c.presets ? iniPath : undefined, reloadStatePath: stateFile, downloadStatePath: path.join(dir, 'dl.json'),
      gateAdminToken: c.gateToken ? 'synthetic-admin-token' : null,
      autoconfig: { modelsPath: c.mount ? models : '', cacheRam: { capMib: 1024, hardMaxMib: 2048 } },
      inferenceBudget: { budgetGib: () => (c.budget === null ? 0 : c.budget) },
    })
    : { enabled: false, kind: 'none', capabilities: {} };
  const sent = [];
  const routes = createModelRoutes({
    json: (res, status, body) => { sent.push({ status, body }); },
    readBody: async (req) => { let s = ''; for await (const x of req) s += x; return s; },
    readJson, fetchJson, env: {}, modelManager,
    getProvider: () => ({}), providerHeaders: () => ({}), DEFAULT_PROVIDER_ID: 'default', createVisionProbe: () => async () => ({}), currentWorkspace: () => ({ userId: 'u' }),
    service: { modelScanCache: new Map(), refreshModelScan() {}, autoRoles: () => null, setAutoRoles() {}, ensureRolesLoaded() {}, servedCatalogue: async () => [], modelsInstalled: async () => [], deriveUserModelName: (x) => x, lastLoadedModel: () => null, clearLastLoadedModel() {}, clearRoleReferences: () => [] },
  });
  const text = c.request.body;
  const req = Object.assign(Readable.from([Buffer.from(text)]), { method: 'PUT', headers: {}, url: '/api/models/preset' });
  const res = { once() {}, off() {}, setHeader() {}, writableEnded: false };
  const url = new URL('http://localhost/api/models/preset');
  // index.cjs: handleRequest(req, res).catch((err) => answerUnhandled(res, err)).
  try { await routes(req, res, { path: url.pathname, authn: { user: { id: 'u1', role: c.request.role } }, url }); }
  catch (err) { const f = errorResponse(err); sent.push({ status: f.status, body: f.body }); }
  await new Promise((resolve) => setTimeout(resolve, 150));
  srv.closeAllConnections?.();
  await new Promise((resolve) => srv.close(resolve));
  const out = sent.at(-1) || null;
  const backups = fs.readdirSync(dir).filter((n) => n.startsWith('models.ini.noevia-backup-')).sort();
  const result = {
    status: out?.status ?? null,
    body: out ? JSON.stringify(out.body) : null,
    router: log.router,
    gate: log.gate,
    ini: fs.readFileSync(iniPath, 'utf8'),
    backups,
    state: fs.existsSync(stateFile) ? fs.readFileSync(stateFile, 'utf8') : null,
  };
  fs.rmSync(dir, { recursive: true, force: true });
  return result;
}

(async () => {
  const cases = [];
  for (const c of S) {
    const request = { ...c.request };
    if (request.big) request.body = '"' + 'a'.repeat(request.big - 2) + '"';
    else request.body = request.body.split('$REV').join(sha(c.ini));
    const expect = await runOne({ ...c, request });
    // The fixture keeps a 1 MiB body as its length (the replay builds the same text).
    if (c.request.big) delete request.body;
    cases.push({ ...c, request, expect });
  }
  process.stdout.write(JSON.stringify({ core: process.env.CORE_REF || null, cases }, null, 1) + '\n');
})().catch((e) => { console.error(e); process.exit(1); });
