'use strict';
// Boot the unmodified pinned Core entrypoint with its own corpus fixtures. No routes, gates,
// response framing or headers are reproduced here. Only execution/storage dependencies are
// synthetic, as in Core's boot tests. EOF on stdin owns cleanup, including during startup.
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const crypto = require('node:crypto');
const { spawn, execFileSync } = require('node:child_process');
const [core, dataDir, readyFile, flag, expectedRef] = process.argv.slice(2);
const actualRef = execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
if (actualRef !== expectedRef) throw new Error(`Core oracle ref mismatch: ${actualRef} != ${expectedRef}`);
if (execFileSync('git', ['-C', core, 'status', '--porcelain', '--untracked-files=no'], { encoding: 'utf8' }).trim()) throw new Error('Core oracle tracked files are dirty');
const { startMocks, serverEnv, SERVER, GUARD } = require(path.join(core, 'tools/contract-corpus/mocks.cjs'));
const storeFixtures = require(path.join(core, 'tests/server/project-store-server.cjs'));
let child, mocks, store, stopping = false;
async function stop(code = 0) {
  if (stopping) return;
  stopping = true;
  if (child && child.exitCode === null) {
    child.kill('SIGTERM');
    await new Promise(resolve => {
      const timer = setTimeout(() => { child.kill('SIGKILL'); resolve(); }, 5000);
      child.once('exit', () => { clearTimeout(timer); resolve(); });
    });
  }
  store?.server.closeAllConnections();
  await store?.close();
  mocks?.close();
  process.exit(code);
}
process.stdin.resume();
process.stdin.on('end', () => stop());
process.on('SIGTERM', () => stop());
process.on('SIGINT', () => stop());
const ready = port => new Promise(resolve => {
  const req = http.get(`http://127.0.0.1:${port}/api/ready`, { agent: false, timeout: 2000 }, res => {
    const chunks = [];
    res.on('data', c => chunks.push(c));
    res.on('end', () => { try { resolve(res.statusCode === 200 && JSON.parse(Buffer.concat(chunks)).ready === true); } catch { resolve(false); } });
  });
  req.on('error', () => resolve(false));
  req.on('timeout', () => { req.destroy(); resolve(false); });
});
(async () => {
  mocks = await startMocks();
  store = await storeFixtures.startProjectStore(dataDir);
  const port = await new Promise(resolve => {
    const probe = http.createServer();
    probe.listen(0, '127.0.0.1', () => { const p = probe.address().port; probe.close(() => resolve(p)); });
  });
  const env = serverEnv({ port, dataDir, origin: 'http://127.0.0.1:18021', mocks });
  Object.assign(env, {
    NOEVIA_PROJECT_STORE_PORT: String(store.port), NOEVIA_PROJECT_STORE_TOKEN: store.token,
    NOEVIA_START_ID: crypto.randomBytes(16).toString('hex'), NOEVIA_FEATURE_CHATGPT_OAUTH: flag,
    DAV_PARSE_WASM_REQUIRED: '1',
  });
  // Older pinned Core uses the store context transport directly; newer fixtures also provide
  // a synthetic authority executable. Use the pinned fixture when it exists.
  if (storeFixtures.syntheticFrontBin) env.NOEVIA_SERVER_BIN = storeFixtures.syntheticFrontBin(dataDir);
  env.NOEVIA_CORPUS_ALLOWED_PORTS += `,${store.port}`;
  child = spawn(process.execPath, ['--require', GUARD, SERVER], { env, cwd: path.dirname(SERVER), stdio: ['ignore', 'ignore', 'pipe'] });
  let log = '';
  child.stderr.on('data', bytes => { log = (log + bytes.toString().split('\n').filter(l => !/SETUP CODE|Setup code file/.test(l)).join('\n')).slice(-4000); });
  const deadline = Date.now() + 90000;
  while (!await ready(port)) {
    if (child.exitCode !== null || Date.now() >= deadline) throw new Error(`Core did not become ready: ${log}`);
    await new Promise(resolve => setTimeout(resolve, 200));
  }
  const { seedAccount } = require(path.join(core, 'tests/server/seed-account.cjs'));
  const accounts = ['admin', 'member'].map(role => seedAccount(dataDir, { username: `synthetic-${role}`, role, publicOrigin: 'http://127.0.0.1:18021' }));
  fs.writeFileSync(`${readyFile}.tmp`, JSON.stringify({ port, accounts, ref: actualRef }));
  fs.renameSync(`${readyFile}.tmp`, readyFile);
})().catch(async error => { console.error(error.stack); await stop(1); });
