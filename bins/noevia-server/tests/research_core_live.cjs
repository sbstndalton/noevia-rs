'use strict';
// A real Core (noevia-core server/index.cjs at contracts/http/front-core.ref) for tests/research_core_live.rs
// (sbstndalton/noevia#2218, review H1). The unit and Stub tests answer the front's lease and bridge calls with
// whatever token they are given, which is how an empty secret in Core's wiring went unseen. This boots the real
// index.cjs with a stand-in project store (Core's own tests/server/project-store-server.cjs), a stand-in
// llama.cpp router that has one model loaded, and a Diary that answers 503, all on 127.0.0.1, over the data
// directory the front uses, seeds one administrator, prints one JSON line {port, token, user, cookie, csrf} when
// /api/ready answers, then runs until stdin ends. Synthetic data only; no model.
//
//   node research_core_live.cjs <core checkout> <data dir> <start id>
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { spawn } = require('node:child_process');

const [core, dataDir, startId] = process.argv.slice(2);
const server = path.join(core, 'server', 'index.cjs');
const { startProjectStore } = require(path.join(core, 'tests', 'server', 'project-store-server.cjs'));

function gguf(kv) {
  const u32 = (n) => { const b = Buffer.alloc(4); b.writeUInt32LE(n); return b; };
  const u64 = (n) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b; };
  const str = (s) => { const b = Buffer.from(s); return Buffer.concat([u64(b.length), b]); };
  const val = (type, v) => (type === 4 ? u32(v) : str(v));
  return Buffer.concat([Buffer.from('GGUF'), u32(3), u64(0), u64(Object.keys(kv).length), ...Object.entries(kv).map(([k, [t, v]]) => Buffer.concat([str(k), u32(t), val(t, v)]))]);
}
const HEADER = { 'general.architecture': [8, 'llama'], 'llama.context_length': [4, 8192], 'llama.embedding_length': [4, 4096], 'llama.block_count': [4, 32], 'llama.attention.head_count': [4, 32], 'llama.attention.head_count_kv': [4, 8], 'tokenizer.chat_template': [8, '{{ messages }}'] };

const listen = (handler) => new Promise((resolve) => { const s = http.createServer(handler); s.listen(0, '127.0.0.1', () => resolve({ server: s, port: s.address().port })); });

async function main() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'research-core-live-'));
  const modelsDir = path.join(root, 'models');
  fs.mkdirSync(modelsDir);
  fs.writeFileSync(path.join(modelsDir, 'q.gguf'), gguf(HEADER));
  const ini = path.join(root, 'models.ini');
  fs.writeFileSync(ini, 'version = 1\n\n[small]\nmodel = /models/q.gguf\nctx-size = 4096\ncache-ram = 0\n');
  let state = 'unloaded';
  const router = await listen((req, res) => {
    const p = new URL(req.url, 'http://router').pathname;
    req.resume();
    req.on('end', () => {
      if (p.startsWith('/gate/')) { res.writeHead(404, { 'content-type': 'application/json' }); res.end(JSON.stringify({ error: { code: 404, message: 'File Not Found', type: 'not_found_error' } })); return; }
      if (p === '/models/load') state = 'loaded';
      if (p === '/models/unload') state = 'unloaded';
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify(p === '/models' ? { data: [{ id: 'small', status: { value: state }, meta: { n_ctx_train: 8192 } }] } : {}));
    });
  });
  const diary = await listen((req, res) => { req.resume(); res.writeHead(503, { 'content-type': 'application/json' }); res.end('{"detail":"synthetic diary"}'); });
  const store = await startProjectStore(dataDir);
  const probe = await listen(() => {}); // a free port for Core's listener: held only to learn it
  const uiPort = probe.port;
  await new Promise((r) => probe.server.close(r));
  const env = {
    PATH: process.env.PATH, NODE_ENV: 'production', TZ: 'UTC', LANG: 'C.UTF-8',
    UI_PORT: String(uiPort), UI_HOST: '127.0.0.1', UI_DATA_DIR: dataDir, PUBLIC_ORIGIN: `http://127.0.0.1:${uiPort}`,
    INFERENCE_BASE_URL: `http://127.0.0.1:${router.port}`, DIARY_BASE_URL: `http://127.0.0.1:${diary.port}`,
    MODEL_LOADER_URL: '', MCP_SERVERS: '', COWORK_DECISION_URL: '', COWORK_SYSTEM_ONE_URL: '', DIARY_BACKUP_WORKER_INTERVAL_MS: '3600000',
    MODEL_MANAGER_KIND: 'llamacpp', MODEL_MANAGER_BASE_URL: `http://127.0.0.1:${router.port}`,
    LLAMACPP_PRESET_PATH: ini, LLAMACPP_MODELS_PATH: modelsDir,
    NOEVIA_PROJECT_STORE_PORT: String(store.port), NOEVIA_PROJECT_STORE_TOKEN: store.token,
    NOEVIA_START_ID: startId || crypto.randomBytes(16).toString('hex'),
    ...(process.env.DAV_PARSE_WASM ? { DAV_PARSE_WASM: process.env.DAV_PARSE_WASM } : {}),
  };
  const child = spawn(process.execPath, [server], { env, cwd: path.dirname(server), stdio: ['ignore', 'pipe', 'pipe'] });
  const log = [];
  child.stdout.on('data', (d) => log.push(String(d)));
  child.stderr.on('data', (d) => log.push(String(d)));
  const stop = async () => {
    child.kill('SIGTERM');
    await new Promise((r) => { const t = setTimeout(r, 5000); child.once('exit', () => { clearTimeout(t); r(); }); });
    if (child.exitCode === null) child.kill('SIGKILL');
    router.server.close(); diary.server.close(); await store.close?.();
    fs.rmSync(root, { recursive: true, force: true });
  };
  const ready = () => new Promise((resolve) => {
    http.get(`http://127.0.0.1:${uiPort}/api/ready`, { agent: false, timeout: 2000 }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => { try { resolve(res.statusCode === 200 && JSON.parse(Buffer.concat(chunks)).ready === true); } catch { resolve(false); } });
    }).on('error', () => resolve(false)).on('timeout', function () { this.destroy(); resolve(false); });
  });
  const started = Date.now();
  for (;;) {
    if (child.exitCode !== null) { process.stderr.write(`core exited during boot:\n${log.join('').slice(-1500)}\n`); await stop(); process.exit(3); }
    if (Date.now() - started > 80000) { process.stderr.write(`core not ready in 80 s:\n${log.join('').slice(-1500)}\n`); await stop(); process.exit(4); }
    if (await ready()) break;
    await new Promise((r) => setTimeout(r, 200));
  }
  // First-run setup is Core's own to hand out on an empty data directory; the account is seeded as Core's tests do.
  const { seedAccount } = require(path.join(core, 'tests', 'server', 'seed-account.cjs'));
  const admin = seedAccount(dataDir, { username: 'synthetic-admin', publicOrigin: 'http://127.0.0.1:18021' });
  process.stdout.write(`${JSON.stringify({ port: uiPort, token: store.token, user: admin.id, cookie: admin.cookie, csrf: admin.csrf })}\n`);
  process.stdin.resume();
  process.stdin.on('end', async () => { await stop(); process.exit(0); });
  process.on('SIGTERM', async () => { await stop(); process.exit(0); });
}
main().catch((e) => { process.stderr.write(`${e?.stack || e}\n`); process.exit(5); });
