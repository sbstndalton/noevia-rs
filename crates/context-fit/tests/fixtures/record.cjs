#!/usr/bin/env node
// Test-oracle glue only: asks noevia-core's own chat-context.cjs, sampling-presets.cjs and thinking-budget.cjs on
// synthetic messages and writes their answers (sbstndalton/noevia#2218).
//   node record.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), os = require('node:os'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const req = (m) => require(path.join(core, 'server', m));
const ctx = req('chat-context.cjs'), sampling = req('sampling-presets.cjs'), budget = req('thinking-budget.cjs');
const u = (c) => ({ role: 'user', content: c }), a = (c) => ({ role: 'assistant', content: c }), s = (c) => ({ role: 'system', content: c });
const tool = (id, c) => ({ role: 'tool', tool_call_id: id, content: c });
const call = (...ids) => ({ role: 'assistant', content: '', tool_calls: ids.map((id) => ({ id, type: 'function', function: { name: 't', arguments: '{}' } })) });
const TOKENS = ['', 'abc', 'abcd', 'é', '😀', null, 7, true, [], {}, u('hi'), [u('hi'), a('there')], { image_url: { url: 'data:xxxx' } }, [{ role: 'user', content: [{ type: 'image_url', image_url: { url: 'x'.repeat(5000) } }, { type: 'text', text: 'hi' }] }]];
const big = (n) => 'x'.repeat(n);
const summarize = (mode) => async (sum, batch) => mode === 'bad' ? '   ' : mode === 'huge' ? big(30000) : mode === 'num' ? 7 : `S(${batch.length})${sum ? '+' : ''}`;
const fail = (e) => ({ error: e.message });
const SCEN = [
  { name: 'fits', m: [s('sys'), u('q'), a('r'), u('q2')], limit: 8192 },
  { name: 'tool-shrink', m: [s('sys'), u('q'), call('c1'), tool('c1', big(40000))], limit: 8192 },
  { name: 'tool-shrink-frame', m: [u('q'), call('c1', 'c2'), tool('c1', `<untrusted kind="x"\n${big(30000)}\n</untrusted>`), tool('c2', 'small')], limit: 8192 },
  { name: 'tool-floor', m: [u(big(30000)), call('c1'), tool('c1', big(40000))], limit: 4096 },
  { name: 'older', m: [s('sys'), u(big(3000)), a(big(3000)), u(big(3000)), a(big(3000)), u('now'), call('c1'), tool('c1', big(2000))], limit: 4096 },
  { name: 'older-tool-floor', m: [u(big(100)), a('r'), u(big(30000)), call('c1'), tool('c1', big(40000))], limit: 4096 },
  { name: 'too-large-older', m: [u(big(20000)), a('r'), u(big(9000))], limit: 8192 },
  { name: 'bad-groups', m: [u(big(90000)), tool('x', 'y')], limit: 8192 },
  { name: 'system-later', m: [u(big(90000)), s('later')], limit: 8192 },
  { name: 'no-user', m: [s('s'), call('c1'), tool('c1', big(60000))], limit: 8192 },
];
const meterOf = (m) => m && ({ limit: m.limit, used: m.used, reserve: m.reserve, safety: m.safety, parts: m.parts, free: m.free, threshold: m.threshold });
(async () => {
  const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim() };
  out.tokens = TOKENS.map((v) => ({ v, out: ctx.tokens(v) }));
  out.measure = [[[s('a'), u('b')], [], 8192], [[u('x')], [{ type: 'function' }], 4096], [[], [], 2048], [[a(big(9000))], [], 32768], [[u('b')], [], 100000]].map(([m, t, l]) => ({ m, t, l, out: meterOf(ctx.measure(m, t, l, 'src', 'model')) }));
  out.budget = [0, 2048, 4096, 12000, 32768, 32769, 100000, 2000000].map((l) => ({ l, out: budget.outputBudget(l) }));
  out.cap = [['low', 4096], ['default', 4096], ['high', 20000], ['nope', 1000], ['high', 1024], ['low', 1536]].map(([e, b]) => ({ e, b, out: budget.thinkingCap(e, b) }));
  out.continuation = [];
  for (const c of SCEN) for (const mode of ['ok', 'bad', 'huge', 'num']) {
    const status = [];
    let r;
    try {
      const got = await ctx.compactContinuation({ messages: c.m, tools: [], limit: c.limit, limitSource: 'x', model: 'm', summarize: summarize(mode), onStatus: (t, k) => status.push([t, k]) });
      r = { messages: got.messages, meter: meterOf(got.meter), compacted: got.compacted, covered: got.covered, truncated: got.truncated };
    } catch (e) { r = fail(e); }
    out.continuation.push({ name: c.name, mode, m: c.m, limit: c.limit, status, out: r });
  }
  out.prepare = [];
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ctx-'));
  const HIST = [u(big(3000)), a(big(3000)), u(big(3000)), a(big(3000)), u(big(1500)), a(big(1500)), u('now')];
  const PREP = [
    { name: 'fits', m: [s('s'), u('q')], limit: 8192 },
    { name: 'compacts', m: [s('s'), ...HIST], limit: 4096 },
    { name: 'force', m: [s('s'), ...HIST], limit: 32768, force: true },
    { name: 'force-nothing', m: [s('s'), u('q'), a('r')], limit: 8192, force: true },
    { name: 'latest-only', m: [s('s'), u(big(2000)), a(big(2000)), u(big(1000)), a(big(1000)), u(big(2400))], limit: 4096 },
    { name: 'tail-too-big', m: [s(big(30000)), u('a'), a('b'), u('c'), a('d'), u('e'), a('f'), u('g')], limit: 4096 },
    { name: 'one-too-large', m: [u(big(20000)), a('b'), u('c'), a('d'), u('e'), a('f'), u('g')], limit: 4096 },
  ];
  for (const c of PREP) for (const mode of ['ok', 'bad']) {
    const id = `${c.name}-${mode}`, status = [];
    let r;
    try {
      const got = await ctx.prepare({ dir, id, messages: c.m, tools: [], limit: c.limit, limitSource: 'x', model: 'm', force: !!c.force, summarize: summarize(mode), onStatus: (t, k) => status.push([t, k]) });
      const st = ctx.read(dir, id);
      r = { messages: got.messages, meter: meterOf(got.meter), maxTokens: got.maxTokens, covered: got.meter.covered, historyCount: got.meter.historyCount, summary: st.summary ?? null, stCovered: st.covered ?? 0, prefix: st.prefix ?? null };
      // A second pass over the same history reads the saved summary.
      const again = await ctx.prepare({ dir, id, messages: c.m, tools: [], limit: c.limit, limitSource: 'x', model: 'm', summarize: async () => { throw Error('no second summary'); } });
      r.again = { messages: again.messages, covered: again.meter.covered };
    } catch (e) { r = fail(e); }
    out.prepare.push({ name: c.name, mode, m: c.m, limit: c.limit, force: !!c.force, status, out: r });
  }
  fs.rmSync(dir, { recursive: true, force: true });
  const keep = [[`head${big(2000)}tail`, 700], ['😀'.repeat(1000), 601], [`<untrusted k="v"\n${big(1000)}\n</untrusted>`, 600], ['short', 600], [`<untrustedly\n${big(1000)}\n</untrusted>`, 600]];
  out.sampling = [];
  for (const [role, message, explicit, autoEnabled] of [
    [null, 'WRITE﻿a short poem', { temperature: 0.1, top_k: 8, model: 'injected' }, true], ['code', 'write a poem', null, true], [null, 'write\u0085a poem', null, true],
    [null, 'write a poem', { temperature: false, top_p: 0, top_k: 2.5, repeat_penalty: 3 }, false], [null, 'rewrite a poem', null, true], ['smart', 'hello', null, true], ['creative', 'x', null, true],
    ['fast', 'brainstorm names', { top_p: 1 }, true], [null, 'creative  writing', { repeat_penalty: 3.1, temperature: 2 }, true], [null, 'x', {}, true], ['smart', 'x', { temperature: 0.7 }, true],
  ]) out.sampling.push({ role, message, explicit, autoEnabled, out: sampling.selectSamplingParams({ routedRole: role, message, explicit, autoEnabled }).params });
  out.groups = [[], [u('a')], [call('c1'), tool('c1', 'x')], [call('c1')], [tool('c1', 'x')], [call('c1', 'c2'), tool('c2', 'x'), tool('c1', 'y')], [{ role: 'assistant', tool_calls: [{ id: '' }] }], [call('c1'), u('x'), tool('c1', 'y')]];
  fs.writeFileSync(process.argv[3], JSON.stringify(out, null, 1) + '\n');
})().catch((e) => { console.error(e); process.exit(1); });
