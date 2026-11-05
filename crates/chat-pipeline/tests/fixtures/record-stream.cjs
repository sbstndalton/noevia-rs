#!/usr/bin/env node
// Test-oracle glue only: runs the chunk loop of noevia-core's own chat.cjs round (its lines, read from chat.cjs: the
// TextDecoder, the line split, the event handling up to the tool upserts) over synthetic engine streams, and records
// every send / markFirstOutput / reportRoundUsage call and the round's state. Synthetic data only.
//   node record-stream.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const source = fs.readFileSync(path.join(core, 'server', 'chat.cjs'), 'utf8');
const START = '          buffer += decoder.decode(chunk, { stream: true });';
const END = '\n        }\n      } catch (err) {';
const a = source.indexOf(START), b = source.indexOf(END, a);
if (a < 0 || b < 0) throw new Error('the round loop moved in chat.cjs');
const body = source.slice(a, b);
const near = (m) => require(m.startsWith('./') ? path.join(core, 'server', m) : m);
const context = near('./chat-context.cjs');
// eslint-disable-next-line no-new-func
const step = new Function('chunk', 'S', 'require', `
  let { buffer, sawAnything, roundReasoning, roundHasContent, shownText, roundContent, roundTimings } = S;
  const { decoder, send, markFirstOutput, reportRoundUsage, context, chatgptProvider, toolCalls, toolOffset, MAX_TOOL_ARGS_UNITS, shownToolName, res } = S;
  try { ${body} } finally { Object.assign(S, { buffer, sawAnything, roundReasoning, roundHasContent, shownText, roundContent, roundTimings }); }`);
let seed = 29;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const d = (delta, extra = {}) => ({ choices: [{ delta, ...extra }], ...extra.top });
const FRAMES = [
  () => d({ content: pick(['Hel', 'lo', ' wörld', '😀', '', 5, null]) }),
  () => d({ reasoning_content: pick(['think', '', 'more']) }),
  () => d({ reasoning: pick(['r', '']) }),
  () => d({ tool_calls: [{ index: pick([0, 1, 2, undefined, '1', -0, 1.5]), id: pick([undefined, 'call_a', '', 7]), function: { name: pick([undefined, 'web_search', 'tavily_search', 'read_', 'file', '']), arguments: pick([undefined, '{"query":"', 'Search the web: latest news. Cite the source', '"}', '{"query":"x"}', '']) } }] }),
  () => d({ tool_calls: [null] }),
  () => d({ tool_calls: [{ index: 0, function: { arguments: '{"a":1}' } }, null, { index: 1, function: { name: 'x' } }] }),
  () => ({ choices: [{ delta: {}, finish_reason: pick(['length', 'stop', null]) }] }),
  () => ({ usage: { prompt_tokens: 5, completion_tokens: pick([3, 0, null]), total_tokens: 8 }, timings: pick([undefined, { predicted_per_second: 12.5 }, { draft_n: 4, draft_n_accepted: 3 }, [1, 2]]) }),
  () => ({ timings: { prompt_ms: pick([1, 2]) } }),
  () => ({ error: pick([{ message: 'context size exceeded', code: 400 }, { message: 'You hit your limit.', source: 'chatgpt' }, 'plain error', { message: 7, source: 'chatgpt' }, { code: 1e20 }, true]) }),
  () => ({ choices: 'str' }), () => null, () => 5, () => ({ choices: { 0: { delta: { content: 'objchoice' } } } }), () => ({ choices: [{ delta: 'x' }] }),
];
const LINE = (f) => pick(['data: ', 'data:', '  data:  ', 'event: x\ndata: ']) + JSON.stringify(f) + pick(['\n', '\n\n', '\r\n']);
const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(), cases: [] };
for (let i = 0; i < 1200; i++) {
  let text = '';
  for (let k = rnd(10); k > 0; k--) text += rnd(12) === 0 ? pick([': keep-alive\n', 'data: [DONE]\n', 'data: {bad json\n', '\n', 'data: "\\ud83d"\n']) : LINE(pick(FRAMES)());
  const bytes = Buffer.from(text, 'utf8');
  if (rnd(6) === 0) { const at = rnd(bytes.length + 1); bytes[at] = 0xff; }
  // Split anywhere, also inside a multi-byte character.
  const cuts = [...new Set(Array.from({ length: rnd(6) }, () => rnd(bytes.length + 1)))].sort((x, y) => x - y);
  const chunks = []; let from = 0; for (const c of [...cuts, bytes.length]) { chunks.push(bytes.subarray(from, c)); from = c; }
  const calls = [];
  const S = { buffer: '', sawAnything: false, roundReasoning: '', roundHasContent: false, shownText: rnd(2) === 0, roundContent: '', roundTimings: null,
    decoder: new TextDecoder(), send: (e) => calls.push(['send', e]), markFirstOutput: () => calls.push(['first']), reportRoundUsage: (u, t) => calls.push(['usage', u, t ?? null]),
    context, chatgptProvider: rnd(2) === 0, toolCalls: new Map(), toolOffset: pick([0, 3]), MAX_TOOL_ARGS_UNITS: 8 * 1024 * 1024, shownToolName: (n) => String(n || '').slice(0, 64), res: { end: () => calls.push(['end']) } };
  const shownBefore = S.shownText;
  let ended = false;
  for (const c of chunks) { if (ended) break; const r = step(c, S, near); if (calls.length && calls[calls.length - 1][0] === 'end') ended = true; void r; }
  out.cases.push({ chunks: chunks.map((c) => c.toString('base64')), chatgpt: S.chatgptProvider, toolOffset: S.toolOffset, shownBefore, calls,
    state: { sawAnything: S.sawAnything, roundReasoning: S.roundReasoning, roundHasContent: S.roundHasContent, shownText: S.shownText, roundContent: S.roundContent,
      tools: [...S.toolCalls.entries()].map(([k, v]) => [k, v]) } });
}
fs.writeFileSync(process.argv[3], JSON.stringify(out));
console.log(`recorded ${out.cases.length}; sends ${out.cases.reduce((n, c) => n + c.calls.length, 0)}`);
