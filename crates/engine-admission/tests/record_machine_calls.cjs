#!/usr/bin/env node
'use strict';
// Recorded machine-calls contract for crates/engine-admission (src/machine_calls.rs):
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/engine-admission/tests/record_machine_calls.cjs > crates/engine-admission/tests/fixtures/machine-calls.v1.json
//
// Every answer is computed by noevia-core's own server/chat-machines.cjs (propsSource, fetchJson, visionFor, busy) and
// server/vision.cjs (createVisionProbe) over a scripted fetch: synthetic, QA-named machines, no network.
// tests/machine_calls_oracle.rs replays it (MACHINE_CALLS_ORACLE_REQUIRED=1 makes a missing file fail). The source
// files' sha256 are recorded.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) { process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n'); process.exit(2); }
const core = path.resolve(checkout);
const FILES = ['server/chat-machines.cjs', 'server/vision.cjs', 'server/http.cjs', 'server/machine-health.cjs'];
const sources = {};
for (const rel of FILES) sources[rel] = crypto.createHash('sha256').update(fs.readFileSync(path.join(core, rel))).digest('hex');
for (const level of ['log', 'info', 'warn', 'error']) console[level] = (...args) => process.stderr.write(`${args.map(String).join(' ')}\n`);

const { createChatMachines } = require(path.join(core, 'server/chat-machines.cjs'));
const { createVisionProbe } = require(path.join(core, 'server/vision.cjs'));
const { createMachineHealth } = require(path.join(core, 'server/machine-health.cjs'));

// An answer: { status, body?, repeat? } (body repeated `repeat` times) | { error } (EPUBLICADDR, ECONNREFUSED,
// TimeoutError, ENOTFOUND, redirect, other). No answer for a URL: 'other'.
function bodyOf(a) { return a.body == null ? null : (a.repeat ? a.body.repeat(a.repeat) : a.body); }
function scripted(answers, calls, during = () => null) {
  return async (url, init = {}) => {
    const headers = {};
    for (const [k, v] of Object.entries(init.headers || {})) headers[k] = String(v);
    calls.push({ url: String(url), method: init.method || 'GET', headers, body: init.body ?? null, redirect: init.redirect ?? null, busy: during() });
    const a = answers[String(url)];
    if (!a || a.error === 'other') throw new Error('socket hang up');
    if (a.error === 'TimeoutError') throw Object.assign(new Error('The operation was aborted due to timeout'), { name: 'TimeoutError' });
    if (a.error === 'redirect') throw new TypeError('unexpected redirect');
    if (a.error) throw Object.assign(new Error(a.error), { code: a.error });
    return new Response(bodyOf(a), { status: a.status });
  };
}
const thrown = (err) => ({ throws: err?.code || err?.name || 'Error' });

const QA1 = { id: 'qa-m1', name: 'QA Pi', address: 'http://qa-pi.lan' };
const QA2 = { id: 'qa-m2', name: 'QA Rock', address: 'http://qa-rock.lan' };
const target = (role, model, machine = QA1, port = 8080) => ({ role, machine, port, model, baseUrl: `${machine.address}:${port}` });
const P = (base, m) => `${base}/props?model=${encodeURIComponent(m)}`;
const BIG_PROPS = 1024 * 1024; // PROPS_BODY_CAP
const BIG_JSON = 256 * 1024; // MODELS_BODY_CAP

// ── 1. propsSource(t).props(model) ─────────────────────────────────────
const T1 = target('smart', 'qa-remote');
const PROPS_CASES = [
  { model: 'qa-remote', answer: { status: 200, body: '{"chat_template":"{{ tools }}","default_generation_settings":{"n_ctx":8192}}' } },
  { model: 'qa remote/\u00fc?&', answer: { status: 200, body: '{"chat_template":""}' } },
  { model: 'qa-remote', answer: { status: 404, body: 'no' } },
  { model: 'qa-remote', answer: { status: 500 } },
  { model: 'qa-remote', answer: { status: 200, body: '' } },
  { model: 'qa-remote', answer: { status: 200, body: 'not json' } },
  { model: 'qa-remote', answer: { status: 200, body: ' [1,2] ' } },
  { model: 'qa-remote', answer: { status: 200, body: 'x', repeat: BIG_PROPS } },
  { model: 'qa-remote', answer: { status: 200, body: 'x', repeat: BIG_PROPS + 1 } },
  { model: 'qa-remote', answer: { error: 'TimeoutError' } },
  { model: 'qa-remote', answer: { error: 'EPUBLICADDR' } },
  { model: 'qa-remote', answer: { error: 'redirect' } },
];

async function propsCase(c) {
  const url = P(T1.baseUrl, c.model);
  const calls = [];
  const m = createChatMachines({ machines: { chatTarget: () => null, machine: () => null }, fetchImpl: scripted({ [url]: c.answer }, calls) });
  let result;
  try { result = await m.propsSource(T1).props(c.model); } catch (err) { result = thrown(err); }
  // A body of the big cases is not kept in the fixture.
  if (result && result.ok && typeof result.body === 'string' && result.body.length > 1000) result = { ...result, body: `<${result.body.length} chars>` };
  return { model: c.model, answer: c.answer, url, result, calls };
}

// ── 2. fetchJson(url, init, timeoutMs): the answer and the machine it counts a turn on ──────
const ROLES2 = { fast: target('fast', 'qa-fast'), smart: target('smart', 'qa-smart'), vision: target('vision', 'qa-v', QA2, 9000) };
const FETCH_CASES = [
  { url: 'http://qa-pi.lan:8080/v1/chat/completions', init: { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: '{"q":1}' }, answer: { status: 200, body: '{"choices":[]}' } },
  { url: 'http://QA-PI.lan:8080/v1/chat/completions', init: { method: 'POST', body: 'x' }, answer: { status: 200, body: 'plain text' } },
  { url: 'http://qa-rock.lan:9000/v1/chat/completions', init: { method: 'POST' }, answer: { status: 500, body: '{"error":{"message":"busy"}}' } },
  { url: 'http://qa-pi.lan:8081/v1/models', init: {}, answer: { status: 404, body: '' } },
  { url: 'http://qa-pi.lan:8080/big', init: {}, answer: { status: 200, body: 'y', repeat: BIG_JSON } },
  { url: 'http://qa-pi.lan:8080/bigger', init: {}, answer: { status: 200, body: 'y', repeat: BIG_JSON + 1 } },
  { url: 'http://qa-pi.lan:8080/v1/x', init: {}, answer: { error: 'ECONNREFUSED' } },
  { url: 'http://qa-pi.lan:8080/v1/y', init: {}, answer: { error: 'redirect' } },
  { url: 'not a url', init: {}, answer: { error: 'other' } },
  { url: 'http://qa-pi.lan:8080/v1/z', init: {}, answer: { status: 200, body: '\ufeff{}' } },
];

async function fetchCase(c) {
  const calls = [];
  let m = null;
  const fetchImpl = scripted({ [c.url]: c.answer }, calls, () => ({ 'qa-m1': m.busy('qa-m1'), 'qa-m2': m.busy('qa-m2') }));
  m = createChatMachines({ machines: { chatTarget: (role) => ROLES2[role] || null, machine: () => null }, fetchImpl });
  let result;
  try { result = await m.fetchJson(c.url, c.init); } catch (err) { result = thrown(err); }
  if (result && typeof result.body === 'string' && result.body.length > 1000) result = { ...result, body: `<${result.body.length} chars>` };
  return { ...c, result, calls, after: { 'qa-m1': m.busy('qa-m1'), 'qa-m2': m.busy('qa-m2') } };
}

// ── 3. vision.cjs createVisionProbe: answers, request bytes, the cache over a clock ─────
const PROBE_URL = 'http://qa-pi.lan:8080/v1/chat/completions';
const PROBE_CASES = [
  { name: 'supported', base: 'http://qa-pi.lan:8080/v1', answers: { [PROBE_URL]: { status: 200, body: '{}' } } },
  { name: 'base without v1, slashes', base: 'http://qa-pi.lan:8080//', answers: { [PROBE_URL]: { status: 200 } } },
  { name: 'mmproj', base: 'http://qa-pi.lan:8080/v1', answers: { [PROBE_URL]: { status: 400, body: '{"error":"image input is not supported - hint: if this is unexpected, you may need to provide the MMPROJ"}' } } },
  { name: 'projector', base: 'http://qa-pi.lan:8080/v1', answers: { [PROBE_URL]: { status: 500, body: 'no Projector loaded' } } },
  { name: 'plain refusal', base: 'http://qa-pi.lan:8080/v1', answers: { [PROBE_URL]: { status: 422, body: 'bad request' } } },
  { name: 'hint past the cap', base: 'http://qa-pi.lan:8080/v1', answers: { [PROBE_URL]: { status: 400, body: `${'z'.repeat(64 * 1024)}mmproj` } } },
  { name: 'timeout', base: 'http://qa-pi.lan:8080/v1', answers: { [PROBE_URL]: { error: 'TimeoutError' } } },
  { name: 'redirect', base: 'http://qa-pi.lan:8080/v1', answers: { [PROBE_URL]: { error: 'redirect' } } },
  { name: 'headers', base: 'http://qa-pi.lan:8080/v1', headers: { Authorization: 'Bearer qa-token', 'Content-Type': 'text/plain' }, answers: { [PROBE_URL]: { status: 200 } } },
];
// Each case: probe at t=0, again at +29 s, +31 s and +301 s (the 30 s / 300 s lifetimes).
async function probeCase(c) {
  let now = 1_000_000;
  const calls = [];
  const probe = createVisionProbe({ fetchImpl: scripted(c.answers, calls), now: () => now });
  const steps = [];
  for (const at of [0, 29_000, 31_000, 301_000]) {
    now = 1_000_000 + at;
    const before = calls.length;
    const result = await probe(c.base, c.headers || {}, 'qa-vision');
    steps.push({ at, result, fetched: calls.length - before });
  }
  return { ...c, steps, calls };
}
const ADMIT = [
  { name: 'admit refuses with a public message', admit: async () => { throw Object.assign(new Error('x'), { publicMessage: 'QA budget full.' }); } },
  { name: 'admit refuses without one', admit: async () => { throw new Error('x'); } },
  { name: 'admit lets it run', admit: async () => {} },
];
async function admitCase(c) {
  const calls = [];
  const probe = createVisionProbe({ fetchImpl: scripted({ [PROBE_URL]: { status: 200 } }, calls), now: () => 1_000_000, admit: c.admit });
  const first = await probe('http://qa-pi.lan:8080/v1', {}, 'qa-vision');
  const second = await probe('http://qa-pi.lan:8080/v1', {}, 'qa-vision');
  return { name: c.name, first, second, fetched: calls.length };
}

// ── 4. visionFor(t): the machine's /props modalities first, else the probe at <base>/v1 ─────
const VT = target('vision', 'qa-v', QA2, 9000);
const VH = 'http://qa-rock.lan:9000/health', VM = 'http://qa-rock.lan:9000/v1/models', VP = P(VT.baseUrl, VT.model);
const VPROBE = 'http://qa-rock.lan:9000/v1/chat/completions';
const VISION_FOR = [
  { name: 'props say vision', answers: { [VH]: { status: 200 }, [VM]: { status: 200, body: '{"data":["qa-v"]}' }, [VP]: { status: 200, body: '{"modalities":{"vision":true}}' } } },
  { name: 'props say no vision', answers: { [VH]: { status: 200 }, [VM]: { status: 200, body: '{"data":["qa-v"]}' }, [VP]: { status: 200, body: '{"modalities":{"vision":false}}' } } },
  { name: 'props silent, probe ok', answers: { [VH]: { status: 200 }, [VM]: { status: 200, body: '{"data":["qa-v"]}' }, [VP]: { status: 200, body: '{}' }, [VPROBE]: { status: 200 } } },
  { name: 'props unreadable, probe mmproj', answers: { [VP]: { status: 500 }, [VPROBE]: { status: 400, body: 'mmproj missing' } } },
  { name: 'nothing answers', answers: {} },
];
async function visionForCase(c) {
  const calls = [];
  const fetchImpl = scripted(c.answers, calls);
  const health = createMachineHealth({ fetchImpl, now: () => 1_000_000 });
  const m = createChatMachines({ machines: { chatTarget: () => null, machine: () => null }, health, fetchImpl, createVisionProbe: (o) => createVisionProbe({ ...o, now: () => 1_000_000 }) });
  const result = await m.visionFor(VT, { 'X-QA': '1' });
  return { name: c.name, answers: c.answers, result, calls };
}

(async () => {
  const props = [];
  for (const c of PROPS_CASES) props.push(await propsCase(c));
  const fetchJson = [];
  for (const c of FETCH_CASES) fetchJson.push(await fetchCase(c));
  const probes = [];
  for (const c of PROBE_CASES) probes.push(await probeCase(c));
  const admits = [];
  for (const c of ADMIT) admits.push(await admitCase(c));
  const visionFor = [];
  for (const c of VISION_FOR) visionFor.push(await visionForCase(c));
  const out = { version: 1, sources, roles: ROLES2, props, fetchJson, probes, admits, visionFor };
  process.stdout.write(`${JSON.stringify(out, null, 1)}\n`);
})().catch((err) => { process.stderr.write(`${err.stack}\n`); process.exit(1); });
