#!/usr/bin/env node
'use strict';
// Recorded machine-turn contract for crates/engine-admission (src/machine_turn.rs, src/machine_health.rs):
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/engine-admission/tests/record_machine_turn.cjs > crates/engine-admission/tests/fixtures/machine-turn.v1.json
//
// Every answer is computed by noevia-core's own server/chat-machines.cjs (createChatMachines, modelIds, propsFactsOf)
// and server/machine-health.cjs (createMachineHealth, healthUrl) over a scripted fetch: synthetic, QA-named machines,
// no network. tests/machine_turn_oracle.rs replays it (MACHINE_TURN_ORACLE_REQUIRED=1 makes a missing file fail).
// The source files' sha256 are recorded.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) { process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n'); process.exit(2); }
const core = path.resolve(checkout);
const FILES = ['server/chat-machines.cjs', 'server/machine-health.cjs'];
const sources = {};
for (const rel of FILES) sources[rel] = crypto.createHash('sha256').update(fs.readFileSync(path.join(core, rel))).digest('hex');
for (const level of ['log', 'info', 'warn', 'error']) console[level] = (...args) => process.stderr.write(`${args.map(String).join(' ')}\n`);

const { createChatMachines, modelIds, propsFactsOf } = require(path.join(core, 'server/chat-machines.cjs'));
const { createMachineHealth, healthUrl } = require(path.join(core, 'server/machine-health.cjs'));

// ── a scripted fetch: url -> { status, body } | { error } (EPUBLICADDR, ECONNREFUSED, TimeoutError, ENOTFOUND, redirect, other)
function scripted(answers, calls) {
  return async (url) => {
    calls.push(String(url));
    const a = answers[String(url)];
    if (!a || a.error === 'other') throw new Error('socket hang up');
    if (a.error === 'TimeoutError') throw Object.assign(new Error('The operation was aborted due to timeout'), { name: 'TimeoutError' });
    if (a.error === 'redirect') throw new TypeError('unexpected redirect');
    if (a.error) throw Object.assign(new Error(a.error), { code: a.error });
    return new Response(a.body ?? null, { status: a.status });
  };
}

const MACHINE = { id: 'qa-m1', name: 'QA Pi', address: 'http://qa-pi.lan' };
const target = (role, model, port = 8080) => ({ role, machine: MACHINE, port, model, baseUrl: `${MACHINE.address}:${port}` });
const PROPS = JSON.stringify({ default_generation_settings: { n_ctx: 16384 }, chat_template: '{% if enable_thinking %}{% endif %}', modalities: { vision: false } });
const H = 'http://qa-pi.lan:8080/health', M = 'http://qa-pi.lan:8080/v1/models', P = (m) => `http://qa-pi.lan:8080/props?model=${encodeURIComponent(m)}`;

// ── 1. pure readers ─────────────────────────────────────────────────────
const PROPS_BODIES = [
  {}, null, { default_generation_settings: { n_ctx: 32768 } }, { default_generation_settings: { n_ctx: '8192' } },
  { default_generation_settings: { n_ctx: ' 0x2000 ' } }, { default_generation_settings: { n_ctx: 2047 } },
  { default_generation_settings: { n_ctx: 2000000 } }, { default_generation_settings: { n_ctx: 2000001 } },
  { default_generation_settings: { n_ctx: 4096.5 } }, { default_generation_settings: { n_ctx: [4096] } },
  { default_generation_settings: { n_ctx: true } }, { chat_template: 'enable_thinking' }, { chat_template: 'my_enable_thinking' },
  { chat_template: 'enable_thinking2' }, { chat_template: 'é-enable_thinking-é' }, { chat_template: '' }, { chat_template: 7 },
  { modalities: { vision: true } }, { modalities: { vision: 'yes' } }, { modalities: null },
];
const props = PROPS_BODIES.map((body) => ({ body, facts: propsFactsOf(body) }));
const MODEL_BODIES = [
  { data: [{ id: 'qa-a' }, 'qa-b', { id: 'qa-a' }, { id: ' ' }, { id: 'qa-c\u0001' }, { id: null, model: 'qa-d' }, { id: 5, model: 'qa-e' }, { name: 'qa-f' }, null] },
  { data: {}, models: [{ name: 'qa-x' }] }, { data: 'no' }, null, { models: ['x'.repeat(200), `${'x'.repeat(199)}😀`] },
  { data: Array.from({ length: 250 }, (_, i) => `qa-m${i}`) },
];
const models = MODEL_BODIES.map((body) => ({ body, ids: modelIds(body) }));
const urls = [[' http://QA-Pi.LAN:8080/v1/x?y ', '/health'], ['https://qa-pi.lan:443', '/health'], ['http://[fd00::1]:9/', '/v1/models'],
  ['ftp://qa-pi.lan', '/health'], ['not a url', '/health']].map(([base, p]) => ({ base, path: p, url: healthUrl(base, p) }));

// ── 2. ready / measured row / connect errors, one fresh instance per scenario ─────
const SCENARIOS = [
  { name: 'up and serving', answers: { [H]: { status: 200 }, [M]: { status: 200, body: '{"data":[{"id":"qa-remote"}]}' }, [P('qa-remote')]: { status: 200, body: PROPS } } },
  { name: 'needs auth is up', answers: { [H]: { status: 401 }, [M]: { status: 200, body: '{"data":["qa-remote"]}' } } },
  { name: 'wrong model', answers: { [H]: { status: 200 }, [M]: { status: 200, body: '{"data":["qa-other-1","qa-other-2","qa-other-3","qa-other-4"]}' } } },
  { name: 'serves nothing', answers: { [H]: { status: 200 }, [M]: { status: 200, body: '{"data":[]}' } } },
  { name: 'models unreadable', answers: { [H]: { status: 200 }, [M]: { status: 200, body: 'not json' } } },
  { name: 'models http 500', answers: { [H]: { status: 200 }, [M]: { status: 500 } } },
  { name: 'down 503', answers: { [H]: { status: 503 } } },
  { name: 'timeout', answers: { [H]: { error: 'TimeoutError' } } },
  { name: 'not on this network', answers: { [H]: { error: 'EPUBLICADDR' } } },
  { name: 'refused', answers: { [H]: { error: 'ECONNREFUSED' } } },
  { name: 'not found', answers: { [H]: { error: 'ENOTFOUND' } } },
  { name: 'redirect', answers: { [H]: { error: 'redirect' } } },
  { name: 'props http 404', answers: { [H]: { status: 200 }, [M]: { status: 200, body: '{"data":["qa-remote"]}' }, [P('qa-remote')]: { status: 404 } } },
];

async function scenario(s) {
  const calls = [];
  const fetchImpl = scripted(s.answers, calls);
  const health = createMachineHealth({ fetchImpl, now: () => 1000000 });
  const machines = createChatMachines({ machines: { chatTarget: () => null, machine: () => null }, health, fetchImpl });
  const t = target('smart', 'qa-remote');
  const ready = await machines.ready(t);
  const row = await machines.measuredProvider(t);
  const measured = await machines.measurements(t);
  return { name: s.name, answers: s.answers, ready, row, measured, calls };
}

const CONNECT = ['EPUBLICADDR', 'redirect', 'TimeoutError', 'other'].map((kind) => {
  const err = kind === 'EPUBLICADDR' ? Object.assign(new Error('refused'), { code: 'EPUBLICADDR' })
    : kind === 'redirect' ? new TypeError('unexpected redirect')
      : kind === 'TimeoutError' ? Object.assign(new Error('x'), { name: 'TimeoutError' }) : new Error('socket hang up');
  return { kind, text: createChatMachines({}).connectError(target('code', 'qa-code'), err) };
});

// ── 3. Manual pick and roles ─────────────────────────────────────────────
const REMOTE = { smart: target('smart', 'qa-remote-a'), code: target('code', 'qa-remote-a'), vision: { ...target('vision', 'qa-remote-v', 9000), machine: { id: 'qa-m2', name: 'QA Rock', address: 'http://qa-rock.lan' }, baseUrl: 'http://qa-rock.lan:9000' } };
const remoteMachines = () => createChatMachines({ machines: { chatTarget: (role) => REMOTE[role] || null, machine: () => null } });
const MANUAL = [
  ['qa-remote-a', [{ name: 'qa-local-s' }]], ['qa-remote-a', [{ name: 'qa-remote-a' }]], ['qa-remote-a', null], ['qa-remote-v', []],
  ['qa-local-s', []], ['', []], ['qa-remote-a', 'throws'],
];

(async () => {
  const ready = [];
  for (const s of SCENARIOS) ready.push(await scenario(s));
  const manual = [];
  for (const [model, rows] of MANUAL) {
    let reads = 0;
    const got = await remoteMachines().manualTarget(model, () => { reads += 1; if (rows === 'throws') throw new Error('unreadable'); return rows; });
    manual.push({ model, catalogue: rows, role: got ? got.role : null, reads });
  }
  const m = remoteMachines();
  const ROLES = [{ fast: 'qa-local-f', smart: 'qa-local-s', creative: 'qa-local-c' }, {}, null];
  const roles = ROLES.map((r) => ({ roles: r, effective: m.effectiveRoles(r) ?? null, local: m.localRoles(r) ?? null }));
  const machineModels = m.machineModels();
  const out = { version: 1, sources, props, models, urls, ready, connect: CONNECT, manual, roles, machineModels };
  process.stdout.write(`${JSON.stringify(out, null, 1)}\n`);
})().catch((err) => { process.stderr.write(`${err.stack}\n`); process.exit(1); });
