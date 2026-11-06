#!/usr/bin/env node
// Test-oracle glue only: calls noevia-core's own chat-machines.cjs (#1840/#1849/#1851/#1854/#1880) with a synthetic
// machine list, health probe and on-network fetch (served models, /props), and records target, remoteRoles,
// effectiveRoles, localRoles, ready, contextTokens, measuredProvider, manualTarget, connectError and the parsers
// modelIds/propsFactsOf. Synthetic only.
//   node record-machines.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const cm = require(path.join(core, 'server', 'chat-machines.cjs'));
let seed = 57;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const ROLES = ['fast', 'smart', 'code', 'creative', 'vision'];
const MODELS = [{ data: [{ id: 'm-a' }, { id: 'm-b' }] }, { data: [{ id: 'm-b' }, 'm-c', { model: 'm-d' }, { name: 'm-a' }, { id: 'm-b' }] }, { models: [{ name: 'm-a' }] }, { data: [] },
  { data: [{ id: 'bad\u0001' }, { id: ' ' }, { id: 'x'.repeat(201) }, { id: 7 }, { id: null, model: 'm-a' }] }, 'fail', 'not-ok', 'not-json', { data: [{ id: 'm1' }, { id: 'm2' }, { id: 'm3' }, { id: 'm4' }] }];
const PROPS = [{ default_generation_settings: { n_ctx: 32768 }, chat_template: '{% if enable_thinking %}', modalities: { vision: true } },
  { default_generation_settings: { n_ctx: '4096' }, chat_template: 'x enable_thinkingx', modalities: { vision: 'yes' } }, { default_generation_settings: { n_ctx: 1024 } },
  { default_generation_settings: { n_ctx: 4096.5 }, chat_template: '' }, { chat_template: 'a_enable_thinking b' }, 'not-ok', 'fail', {}, { modalities: { vision: false }, default_generation_settings: { n_ctx: 2000001 } }];
const HEALTH = [{ state: 'up' }, { state: 'up' }, { state: 'down', reason: 'http-503' }, { state: 'down' }, { state: 'unreachable', reason: 'timeout' }, { state: 'unreachable', reason: 'not-on-this-network' }, 'throw', {}, { state: 'busy', reason: 'x' }];
const t = (role, k) => ({ role, model: pick(['m-a', 'm-b', 'm-c']), port: pick([8080, 9000]), baseUrl: `http://10.0.0.${k}:${8080}`, machine: { id: `mach-${k}`, name: pick(['Rig', 'Box “2”']), address: `http://10.0.0.${k}` } });
(async () => {
  const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), units: { models: MODELS.filter((m) => typeof m === 'object').map((m) => [m, cm.modelIds(m)]), props: PROPS.filter((p) => typeof p === 'object').map((p) => [p, cm.propsFactsOf(p)]) }, cases: [] };
  for (let i = 0; i < 1200; i++) {
    const targets = {}; for (const r of ROLES) if (rnd(3) === 0) targets[r] = t(r, rnd(3) + 1);
    const sc = { hasMachines: rnd(10) > 0, throws: rnd(15) === 0, hasHealth: rnd(8) > 0, targets, health: pick(HEALTH), models: pick(MODELS), props: pick(PROPS),
      roles: pick([null, { fast: 'l-f', smart: 'l-s' }, { fast: 'l-f', smart: 'l-s', code: 'l-c', vision: 'l-v' }]), ask: pick(ROLES), manual: pick([null, '', 'm-a', 'm-b', 'm-z']),
      catalogue: pick(['throw', null, [{ name: 'm-a' }], [{ name: 'other' }], [null]]), err: pick([{ code: 'EPUBLICADDR' }, { message: 'unexpected redirect' }, { name: 'TimeoutError' }, {}, null]) };
    const machines = sc.hasMachines ? { chatTarget: (role) => { if (sc.throws) throw new Error('x'); return sc.targets[role] || null; } } : null;
    const health = sc.hasHealth ? { probe: async () => { if (sc.health === 'throw') throw new Error('h'); return sc.health; }, remember: async (_k, load) => load(null) } : null;
    const reply = (b) => b === 'fail' ? Promise.reject(Object.assign(new Error('net'), { code: 'ECONNREFUSED' }))
      : Promise.resolve(new Response(b === 'not-json' ? '{' : JSON.stringify(b), { status: b === 'not-ok' ? 500 : 200 }));
    const fetchImpl = (url) => (String(url).includes('/v1/models') ? reply(sc.models) : reply(sc.props));
    const m = cm.createChatMachines({ machines, health, fetchImpl });
    let catalogueReads = 0;
    const r = {
      target: m.target(sc.ask), remote: m.remoteRoles(), effective: m.effectiveRoles(sc.roles), local: m.localRoles(sc.roles),
      ready: await m.ready(m.target(sc.ask)),
      context: m.target(sc.ask) ? await m.contextTokens(m.target(sc.ask)) : null,
      provider: m.target(sc.ask) ? await m.measuredProvider(m.target(sc.ask)) : null,
      manual: await m.manualTarget(sc.manual, async () => { catalogueReads++; if (sc.catalogue === 'throw') throw new Error('c'); return sc.catalogue; }),
      connect: m.target(sc.ask) ? m.connectError(m.target(sc.ask), sc.err) : null,
    };
    out.cases.push({ ...sc, out: { ...r, catalogueReads } });
  }
  fs.writeFileSync(process.argv[3], JSON.stringify(out));
  const k = {}; for (const c of out.cases) { const x = c.out.ready.ok ? 'ok' : c.out.ready.code; k[x] = (k[x] || 0) + 1; }
  console.log(JSON.stringify(k));
})();
