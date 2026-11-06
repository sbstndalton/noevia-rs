#!/usr/bin/env node
// Test-oracle glue only: calls noevia-core's own route-preload.cjs (#1863: preloadTarget, startPreload and
// releaseIfUnused) with synthetic roles and a synthetic model manager (listings, load and unload answers), and
// records each decision and the manager calls it made, in order. Synthetic only.
//   node record-preload.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const { preloadTarget, startPreload } = require(path.join(core, 'server', 'route-preload.cjs'));
let seed = 13;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const M = ['m-a', 'm-b', '', undefined];
const ROW = (id, value, loaded) => ({ id, ...(loaded ? { loaded: true } : {}), ...(value === undefined ? {} : { status: { value } }) });
const LISTS = [null, { ok: false }, { ok: true, body: {} }, { ok: true, body: { data: [] } }, { ok: true, body: { data: [ROW('m-a', 'unloaded')] } },
  { ok: true, body: { data: [ROW('m-a', 'unloaded'), ROW('m-b', 'unloaded')] } }, { ok: true, body: { data: [ROW('m-a', 'unloaded'), ROW('m-b', 'loading')] } },
  { ok: true, body: { data: [ROW('m-a', 'unloaded'), ROW('m-b', undefined, true)] } }, { ok: true, body: { data: [ROW('m-a', 'loaded')] } },
  { ok: true, body: { data: [ROW('m-a', 'unloaded', true)] } }, { ok: true, body: { data: [ROW('m-a', 'sleeping'), ROW('m-b', 'sleeping')] } },
  { ok: true, body: { data: [ROW('m-a', 'unloaded'), ROW('m-b', 'sleeping')] } }];
(async () => {
  const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), targets: [], starts: [] };
  for (let i = 0; i < 1500; i++) {
    const roles = rnd(8) === 0 ? null : { fast: pick(M), smart: pick(M), code: pick([undefined, undefined, 'm-a', 'm-c']), creative: pick([undefined, undefined, 'm-a', 'm-c']) };
    const remote = ['fast', 'smart', 'code', 'creative'].filter(() => rnd(6) === 0);
    const likely = rnd(2) === 0, hasImages = rnd(10) === 0, contextHigh = rnd(10) === 0;
    let asked = 0;
    const target = preloadTarget({ roles, remoteRole: (r) => remote.includes(r), hasImages, contextHigh, likelyCode: () => { asked++; return likely; } });
    out.targets.push({ roles, remote, likely, hasImages, contextHigh, target, asked });
  }
  for (let i = 0; i < 2000; i++) {
    const calls = [];
    const listings = [rnd(3) ? pick([LISTS[4], LISTS[5], LISTS[11]]) : pick(LISTS), pick(LISTS), pick(LISTS), pick(LISTS), pick(LISTS)];
    const sc = { model: pick(['m-a', 'm-a', 'm-a', 'm-b', '']), kind: pick(['llamacpp', 'llamacpp', 'other']), busyAt: pick([-1, -1, -1, 0, 1]), abortedAt: pick([-1, -1, -1, 0, 1]),
      load: pick(['ok', 'ok', 'fail', 'throw']), unload: pick(['ok', 'fail', 'none']), used: rnd(5) === 0, idle: rnd(4) > 0, release: rnd(3) > 0, listings, loadingLoops: rnd(3) };
    let busyN = 0, listN = 0;
    const controller = new AbortController();
    const manager = { kind: sc.kind,
      listModels: async () => { let l = sc.listings[Math.min(listN, 4)]; listN++;
        if (sc.release && calls.some((c) => c === 'load') && listN - 2 < sc.loadingLoops) l = { ok: true, body: { data: [ROW(sc.model, 'loading')] } };
        calls.push({ list: l }); return l; },
      load: async () => { calls.push('load'); if (sc.load === 'throw') throw new Error('x'); return { ok: sc.load === 'ok', status: 503 }; },
      ...(sc.unload === 'none' ? {} : { unload: async () => { calls.push('unload'); return { ok: sc.unload === 'ok' }; } }) };
    const busy = () => { const b = busyN === sc.busyAt; if (busyN === sc.abortedAt) controller.abort(); busyN++; return b; };
    const pre = await startPreload({ manager, model: sc.model, signal: controller.signal, busy, log: () => {} });
    let settled = null, released = null;
    if (pre) {
      if (sc.used) pre.use();
      settled = await pre.settled;
      if (sc.release) released = await pre.releaseIfUnused({ idle: () => sc.idle, sleep: async () => { calls.push('sleep'); }, settleMs: 0, waitMs: 1e9 });
    }
    out.starts.push({ ...sc, started: !!pre, settled, released, calls });
  }
  fs.writeFileSync(process.argv[3], JSON.stringify(out));
  const k = {}; for (const s of out.starts) { const x = `${s.started}/${s.settled}/${s.released}`; k[x] = (k[x] || 0) + 1; }
  console.log(JSON.stringify(k));
})();
