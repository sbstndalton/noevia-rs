'use strict';
// Oracle recorder for the model folder sync worker (Core server/model-folder-sync.cjs at origin/main,
// before the worker moved to model_folder_sync.rs).
//
//   node folder-sync-oracle.cjs <core-root-at-base> <out.json>
//
// Synthetic only. Each scenario gets a temp directory of empty synthetic `.gguf` files and a fake
// model-management service (HTTP, loopback) that lists the stems with no settings and answers
// `POST /api/v1/sections/<stem>/safe-defaults` with the status the scenario scripts for that stem.
// The worker is wired as Core's index.cjs wired it: the same two loader calls, the same optional
// held writer, the same busy check. The fake engine is the `reload` / `hold` event log.
// What is recorded per run: the result (without its clock), the ordered events and the state file.
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');

const core = path.resolve(process.argv[2] || '');
const out = process.argv[3];
if (!core || !out) { console.error('usage: folder-sync-oracle.cjs <core-root> <out.json>'); process.exit(2); }
const { createFolderSync } = require(path.join(core, 'server', 'model-folder-sync.cjs'));

const stems = (n, prefix) => Array.from({ length: n }, (_, i) => `${prefix}-${String(i).padStart(3, '0')}`);

// outcomes: stem -> 'ok' | HTTP status | ['502', 'ok'] (a script per attempt)
const SCENARIOS = [
  { name: 'two-new-files-one-reload', files: ['new-a', 'new-b'], outcomes: {}, runs: [{}, { drop: ['new-a', 'new-b'] }] },
  { name: 'refused-files-are-remembered', files: ['draft-head', 'taken', 'gone', 'fine'], outcomes: { 'draft-head': 400, taken: 409, gone: 404 }, runs: [{}, {}, { restart: true }] },
  { name: 'transient-failure-is-retried', files: ['later'], outcomes: { later: [502, 'ok'] }, runs: [{}, {}] },
  { name: 'busy-before-scan', files: ['x', 'y'], outcomes: {}, busy: true, runs: [{}] },
  { name: 'busy-flips-mid-scan', files: ['new-a', 'new-b'], outcomes: {}, flipBusyAfter: 1, runs: [{}, { busy: false }] },
  { name: 'busy-check-throws', files: ['x'], outcomes: {}, busy: 'throw', runs: [{}] },
  { name: 'held-writer-keeps-the-hold-across-the-batch', files: ['one', 'two'], outcomes: {}, writer: 'ok', runs: [{}] },
  { name: 'held-writer-refused-is-transient', files: ['one', 'two'], outcomes: {}, writer: 'refused', runs: [{}, { writer: 'ok' }] },
  { name: 'held-writer-all-refused-no-reload', files: ['draft-a', 'draft-b'], outcomes: { 'draft-a': 400, 'draft-b': 409 }, writer: 'ok', runs: [{}] },
  { name: 'held-writer-other-error-is-a-scan-failure', files: ['one'], outcomes: {}, writer: 'throws', runs: [{}] },
  { name: 'scan-failure-is-logged', files: ['x'], outcomes: {}, listStatus: 500, runs: [{}] },
  { name: 'engine-will-reload-later', files: ['a'], outcomes: {}, reloadOk: false, runs: [{}] },
  { name: 'reload-throws-is-pending', files: ['a'], outcomes: {}, reloadOk: 'throw', runs: [{}] },
  { name: 'skip-list-keeps-the-last-500', files: stems(503, 'draft'), outcomes: Object.fromEntries(stems(503, 'draft').map((s) => [s, 400])), runs: [{}] },
];

async function record(sc) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'folder-sync-oracle-'));
  const models = path.join(dir, 'models');
  fs.mkdirSync(models);
  for (const f of sc.files) fs.writeFileSync(path.join(models, `${f}.gguf`), 'GGUF');
  const stateFile = path.join(dir, 'model-folder-sync.json');
  let events = [], registered = new Set(), attempts = {};
  let busy = sc.busy ?? false, writer = sc.writer || null, registeredCount = 0;
  const server = http.createServer((req, res) => {
    const send = (code, body) => { res.writeHead(code, { 'content-type': 'application/json' }); res.end(JSON.stringify(body)); };
    const url = new URL(req.url, 'http://x');
    if (req.method === 'GET' && url.pathname === '/api/v1/models') {
      events.push('list');
      if (sc.listStatus) return send(sc.listStatus, { error: 'models folder unreadable' });
      const unregistered = fs.readdirSync(models).filter((n) => n.endsWith('.gguf')).map((n) => n.slice(0, -5)).filter((s) => !registered.has(s)).sort();
      return send(200, { models: [], unregistered, revision: 'r' });
    }
    const m = url.pathname.match(/^\/api\/v1\/sections\/([^/]+)\/safe-defaults$/);
    if (req.method === 'POST' && m) {
      const stem = decodeURIComponent(m[1]);
      events.push(`register:${stem}`);
      const script = [].concat(sc.outcomes[stem] ?? 'ok');
      const n = attempts[stem] = (attempts[stem] || 0) + 1;
      const o = script[Math.min(n, script.length) - 1];
      if (o === 'ok') { registered.add(stem); registeredCount++; if (sc.flipBusyAfter && registeredCount >= sc.flipBusyAfter) busy = true; return send(200, { ok: true, revision: 'r' }); }
      return send(o, { error: `scripted ${o}` });
    }
    send(404, { error: 'nope' });
  });
  await new Promise((r) => server.listen(0, '127.0.0.1', r));
  const base = `http://127.0.0.1:${server.address().port}`;
  const call = async (rest, method = 'GET') => {
    try {
      const r = await fetch(`${base}/api/v1/${rest}`, { method, headers: { 'content-type': 'application/json' }, ...(method === 'POST' ? { body: '{}' } : {}) });
      return { ok: r.ok, status: r.status, body: await r.json().catch(() => null) };
    } catch { return null; }
  };
  const logs = [];
  // `writePresets` is read once at creation in index.cjs; build the option accordingly.
  const build = () => {
    const w = writer;
    const sync = createFolderSync({
      stateFile,
      listUnregistered: async () => {
        const r = await call('models');
        if (!r?.ok) throw Object.assign(Error(r?.body?.error || `model manager HTTP ${r?.status || 'unreachable'}`), { status: r?.status });
        return Array.isArray(r.body?.unregistered) ? r.body.unregistered : [];
      },
      register: async (stem) => {
        const r = await call(`sections/${encodeURIComponent(stem)}/safe-defaults`, 'POST');
        if (!r?.ok) throw Object.assign(Error(r?.body?.error || `model manager HTTP ${r?.status || 'unreachable'}`), { status: r?.status });
      },
      reloadPresets: async () => { events.push('reload'); if (sc.reloadOk === 'throw') throw Error('engine down'); return { ok: sc.reloadOk !== false }; },
      writePresets: w ? async (write, reloadIf) => {
        if (w === 'refused') throw Object.assign(Error('busy'), { status: 409, holdRefused: true });
        if (w === 'throws') throw Error('gate exploded');
        events.push('hold');
        try {
          const written = await write();
          if (!reloadIf(written)) return { written, reload: null };
          events.push('reload');
          return { written, reload: { ok: sc.reloadOk !== false } };
        } finally { events.push('release'); }
      } : null,
      busy: () => { if (busy === 'throw') throw Error('no manager'); return !!busy; },
      log: (m) => logs.push(m),
    });
    return sync;
  };
  let sync = build();
  const runs = [];
  try {
    for (const step of sc.runs) {
      if (step.busy !== undefined) busy = step.busy;
      if (step.writer !== undefined) { writer = step.writer; sync = build(); }
      if (step.restart) sync = build();
      if (step.drop) for (const d of step.drop) registered.add(d);
      events = []; logs.length = 0;
      const result = await sync.run();
      const { at, ...rest } = result;
      let state = null;
      try { state = JSON.parse(fs.readFileSync(stateFile, 'utf8')); } catch { /* none */ }
      if (state?.skip && state.skip.length > 20) state = { skip: { count: state.skip.length, first: state.skip[0], last: state.skip.at(-1) } };
      const shown = { ...rest };
      if (shown.skipped && shown.skipped.length > 20) shown.skipped = { count: shown.skipped.length, first: shown.skipped[0], last: shown.skipped.at(-1) };
      const ev = events.length > 60 ? [...events.slice(0, 6), `…${events.length - 6} more`] : events.slice();
      runs.push({ result: shown, hadClock: typeof at === 'number', events: ev, logs: logs.slice(), state, skipCount: sync.skipped().length });
    }
  } finally { server.close(); fs.rmSync(dir, { recursive: true, force: true }); }
  return { name: sc.name, spec: { files: sc.files.length > 20 ? { count: sc.files.length } : sc.files, outcomes: sc.files.length > 20 ? 'all 400' : sc.outcomes, busy: sc.busy ?? false, flipBusyAfter: sc.flipBusyAfter ?? null, writer: sc.writer ?? null, reloadOk: sc.reloadOk ?? true, listStatus: sc.listStatus ?? null, runs: sc.runs }, runs };
}

(async () => {
  const rows = [];
  for (const sc of SCENARIOS) rows.push(await record(sc));
  fs.writeFileSync(out, JSON.stringify({ schema: 'noevia-folder-sync-oracle/1', node: process.version, rows }, null, 2) + '\n');
  console.log(`recorded ${rows.length} scenarios -> ${out}`);
})().catch((e) => { console.error(e); process.exit(1); });
