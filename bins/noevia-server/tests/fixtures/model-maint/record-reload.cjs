'use strict';
// Records Core's own answers for POST /api/models/presets/reload (the real routes/models.cjs and
// llamacpp-manager.cjs of the Core at CORE, before the route moved) against a scripted router and
// inference gate on one port, for synthetic models.ini texts. tests/model_reload.rs replays the same
// scenarios through the Rust front and compares status, body text, the requests the router and the gate
// saw, the models.ini text and the reload guard state file. Synthetic data only; no engine is touched.
//
//   CORE=/path/to/core node record-reload.cjs > reload-cases.json
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
const { fetchJson } = server('http.cjs');
const { engineView } = server('preset-reload.cjs');

const INI = '[*]\ncache-ram = 512\n\n[A]\nmodel = /models/a.gguf\nctx-size = 4096\n\n[B]\nmodel = /models/b.gguf\n';
const INI_B_CHANGED = INI.replace('b.gguf', 'b2.gguf');
const INI_A_CHANGED = INI.replace('4096', '8192');
const row = (id, status, preset = 'p-' + id) => ({ id, status: { value: status, preset, args: ['--model', '/models/' + id + '.gguf'] } });
const rowsAB = (a, b) => [row('A', a), row('B', b)];
const listing = (rows) => ({ status: 200, rows });
// A baseline the previous reload left, for the router rows `rows` and the ini text `text`.
const baseline = (text, rows) => ({ text, view: engineView(rows) });

const S = [];
const add = (name, c) => S.push({ name, ini: INI, kind: 'llamacpp', gateToken: true, gate: 'grant', router: { listings: [listing(rowsAB('unloaded', 'unloaded'))] }, request: { role: 'admin', body: '{}' }, state: null, ...c });

add('nothing loaded: reload and record the baseline', {});
add('nothing loaded, no body at all', { request: { role: 'admin', body: '' } });
add('nothing loaded, unload:true is the same', { request: { role: 'admin', body: '{"unload":true}' } });
add('a model loaded and no baseline refuses', { router: { listings: [listing(rowsAB('loaded', 'unloaded'))] } });
add('a model loaded, baseline unchanged: reload keeps it', { router: { listings: [listing(rowsAB('loaded', 'unloaded'))] }, state: baseline(INI, rowsAB('loaded', 'unloaded')) });
add('a model sleeping counts as loaded', { router: { listings: [listing(rowsAB('sleeping', 'unloaded'))] }, state: baseline(INI, rowsAB('sleeping', 'unloaded')) });
add('a loaded model whose section changed refuses', { ini: INI_A_CHANGED, router: { listings: [listing(rowsAB('loaded', 'unloaded'))] }, state: baseline(INI, rowsAB('loaded', 'unloaded')) });
add('another model changed: the loaded one is kept', { ini: INI_B_CHANGED, router: { listings: [listing(rowsAB('loaded', 'unloaded'))] }, state: baseline(INI, rowsAB('loaded', 'unloaded')) });
add('the router moved since the baseline', { router: { listings: [listing([row('A', 'loaded', 'other'), row('B', 'unloaded')])] }, state: baseline(INI, rowsAB('loaded', 'unloaded')) });
add('a tombstone baseline is unknown', { router: { listings: [listing(rowsAB('loaded', 'unloaded'))] }, state: '{}' });
add('an unreadable baseline file is unknown', { router: { listings: [listing(rowsAB('loaded', 'unloaded'))] }, state: 'not json' });
add('a loaded model and unload:true unloads it first', { request: { role: 'admin', body: '{"unload":true}' }, router: { listings: [listing(rowsAB('loaded', 'loading'))] } });
add('a model that started loading between the listings refuses', { router: { listings: [listing(rowsAB('loaded', 'unloaded')), listing(rowsAB('loaded', 'loading'))] }, state: baseline(INI, rowsAB('loaded', 'unloaded')) });
add('every model unloaded between the listings still reloads', { router: { listings: [listing(rowsAB('loaded', 'unloaded')), listing(rowsAB('unloaded', 'unloaded'))] }, state: baseline(INI, rowsAB('loaded', 'unloaded')) });
add('the second listing fails', { router: { listings: [listing(rowsAB('loaded', 'unloaded')), { status: 500, text: 'boom' }] }, state: baseline(INI, rowsAB('loaded', 'unloaded')) });
add('the second listing is not a listing', { router: { listings: [listing(rowsAB('loaded', 'unloaded')), { status: 200, text: '{"data":"x"}' }] }, state: baseline(INI, rowsAB('loaded', 'unloaded')) });
add('models.ini edited after it was judged refuses (file_changed)', { router: { listings: [listing(rowsAB('loaded', 'unloaded'))], iniOnReloadCheck: INI + '; edited\n' }, state: baseline(INI, rowsAB('loaded', 'unloaded')) });
add('models.ini with a duplicate section cannot be judged', { ini: INI + '\n[A]\nmodel = /models/a2.gguf\n', router: { listings: [listing(rowsAB('loaded', 'unloaded'))] }, state: baseline(INI, rowsAB('loaded', 'unloaded')) });
add('the router listing fails: its answer is passed on', { router: { listings: [{ status: 503, text: '{"error":"router down"}' }] } });
add('the router listing is plain text', { router: { listings: [{ status: 502, text: 'Bad Gateway' }] } });
add('the router listing is ok but has no data array', { router: { listings: [{ status: 200, text: '{"models":[]}' }] } });
add('the router refuses the reload', { router: { listings: [listing(rowsAB('unloaded', 'unloaded'))], reload: { status: 500, text: '{"error":"x"}' } }, state: baseline(INI, rowsAB('unloaded', 'unloaded')) });
add('the router drops the reload connection', { router: { listings: [listing(rowsAB('unloaded', 'unloaded'))], reload: { drop: true } }, state: baseline(INI, rowsAB('unloaded', 'unloaded')) });
add('the listing after the reload has no presets: baseline forgotten', { router: { listings: [listing(rowsAB('unloaded', 'unloaded')), { status: 200, text: '{"data":[{"id":"A","status":{"value":"unloaded"}}]}' }] } });
add('the file moved during the reload: baseline forgotten', { router: { listings: [listing(rowsAB('unloaded', 'unloaded'))], iniAfterReload: INI + '; moved\n' }, state: baseline(INI, rowsAB('unloaded', 'unloaded')) });
add('unload failures are ignored', { request: { role: 'admin', body: '{"unload":true}' }, router: { listings: [listing(rowsAB('loaded', 'loaded'))], unload: { status: 500, text: 'no' } } });
add('the gate says the engine is busy', { gate: { refuse: { status: 409, code: 'engine_busy', message: 'The engine is busy.' } } });
add('another client holds the gate', { gate: { refuse: { status: 503, code: 'maintenance_held', message: 'Held.' } } });
add('the gate rejects the admin token', { gate: { refuse: { status: 403, code: 'admin_required', message: 'The maintenance hold needs the gate\'s admin token.' } } });
add('the gate does not answer', { gate: 'drop' });
add('no gate at the engine address: this process holds alone', { gate: 'absent' });
add('no admin token: this process holds alone', { gateToken: false });
add('a member is refused', { request: { role: 'member', body: '{}' } });
add('invalid JSON', { request: { role: 'admin', body: '{nope' } });
add('a JSON array body', { request: { role: 'admin', body: '[1]' } });
add('null body', { request: { role: 'admin', body: 'null' } });
add('unload:"true" is not true', { request: { role: 'admin', body: '{"unload":"true"}' }, router: { listings: [listing(rowsAB('loaded', 'unloaded'))] } });
add('an engine without a preset file', { kind: 'none' });

async function runOne(c) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'reload-rec-'));
  const iniPath = path.join(dir, 'models.ini');
  fs.writeFileSync(iniPath, c.ini);
  const stateFile = path.join(dir, 'native-preset-reload-x.json');
  if (c.state !== null) fs.writeFileSync(stateFile, typeof c.state === 'string' ? c.state : JSON.stringify(c.state));
  const log = { router: [], gate: [] };
  let listingCalls = 0, reloadCalled = false;
  const r = c.router || {};
  const srv = http.createServer((req, res) => {
    let body = '';
    req.on('data', (d) => { body += d; });
    req.on('end', () => {
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
      if (req.method === 'GET' && url === '/models') {
        log.router.push(`GET /models ${hold}`);
        const i = Math.min(listingCalls++, r.listings.length - 1);
        const l = r.listings[i];
        if (r.iniOnReloadCheck && listingCalls === 2) fs.writeFileSync(iniPath, r.iniOnReloadCheck);
        return send(l.status, l.text !== undefined ? l.text : JSON.stringify({ data: l.rows }));
      }
      if (req.method === 'GET' && url === '/models?reload=1') {
        log.router.push(`GET /models?reload=1 ${hold}`);
        reloadCalled = true;
        if (r.iniAfterReload) fs.writeFileSync(iniPath, r.iniAfterReload);
        if (r.reload && r.reload.drop) { req.socket.destroy(); return; }
        const rl = r.reload || { status: 200, text: '{"success":true}' };
        return send(rl.status, rl.text);
      }
      if (req.method === 'POST' && url === '/models/unload') {
        log.router.push(`POST /models/unload ${hold} ${body}`);
        const u = r.unload || { status: 200, text: '{"success":true}' };
        return send(u.status, u.text);
      }
      log.router.push(`${req.method} ${url} ${hold}`);
      return send(404, '{}');
    });
  });
  await new Promise((resolve) => srv.listen(0, '127.0.0.1', resolve));
  const port = srv.address().port;
  const llama = c.kind === 'llamacpp';
  const modelManager = llama
    ? createLlamaCppManager({ baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 'local', fetchJson, presetPath: iniPath, reloadStatePath: stateFile, downloadStatePath: path.join(dir, 'dl.json'), gateAdminToken: c.gateToken ? 'synthetic-admin-token' : null })
    : { enabled: false, kind: 'none', capabilities: {} };
  const sent = [];
  const routes = createModelRoutes({
    json: (res, status, body) => { sent.push({ status, body }); },
    readBody: async (req) => { let s = ''; for await (const x of req) s += x; return s; },
    readJson: async (req) => { let s = ''; for await (const x of req) s += x; try { return s ? JSON.parse(s) : {}; } catch { throw Object.assign(new SyntaxError('invalid JSON'), { status: 400 }); } },
    fetchJson, env: {}, modelManager,
    getProvider: () => ({}), providerHeaders: () => ({}), DEFAULT_PROVIDER_ID: 'default', createVisionProbe: () => async () => ({}), currentWorkspace: () => ({ userId: 'u' }),
    service: { modelScanCache: new Map(), refreshModelScan() {}, autoRoles: () => null, setAutoRoles() {}, ensureRolesLoaded() {}, servedCatalogue: async () => [], modelsInstalled: async () => [], deriveUserModelName: (x) => x, lastLoadedModel: () => null, clearLastLoadedModel() {}, clearRoleReferences: () => [] },
  });
  const req = Object.assign(Readable.from([Buffer.from(c.request.body)]), { method: 'POST', headers: {}, url: '/api/models/presets/reload' });
  const res = { once() {}, off() {}, setHeader() {}, writableEnded: false };
  const url = new URL('http://localhost/api/models/presets/reload');
  await routes(req, res, { path: url.pathname, authn: { user: { id: 'u1', role: c.request.role } }, url });
  await new Promise((resolve) => setTimeout(resolve, 150));
  srv.closeAllConnections?.();
  await new Promise((resolve) => srv.close(resolve));
  const out = sent.at(-1) || null;
  const stateAfter = fs.existsSync(stateFile) ? fs.readFileSync(stateFile, 'utf8') : null;
  const result = {
    status: out?.status ?? null,
    body: out ? JSON.stringify(out.body) : null,
    router: log.router,
    gate: log.gate,
    ini: fs.readFileSync(iniPath, 'utf8'),
    state: stateAfter,
  };
  fs.rmSync(dir, { recursive: true, force: true });
  return result;
}

(async () => {
  const cases = [];
  for (const c of S) cases.push({ ...c, expect: await runOne(c) });
  process.stdout.write(JSON.stringify({ core: process.env.CORE_REF || null, cases }, null, 1) + '\n');
})().catch((e) => { console.error(e); process.exit(1); });
