#!/usr/bin/env node
// Test-oracle glue only: asks noevia-core's own chat-title.cjs about synthetic messages and model answers, and records
// the exact body generateTitle sends (a fake fetch). Synthetic data only.
//   node record-title.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const t = require(path.join(core, 'server', 'chat-title.cjs'));
let seed = 13;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const WORDS = ['train', 'times', 'QA-1015-T1', 'Say', 'hello', 'é', '😀', 'über', ' ', ' ', '  ', '\n', '\t', 'x'.repeat(30), '日本語', 'a.', '!', '…', '\ud83d', 'ſ', 'K'];
const sentence = (n) => Array.from({ length: n }, () => pick(WORDS)).join(pick([' ', '', ' ']));
const messages = ['', '   ', 'hi', 'x'.repeat(80), 'x'.repeat(81), 'x'.repeat(78) + '😀', 'a '.repeat(50), ' padded message ', null, 7];
for (let i = 0; i < 600; i++) messages.push(sentence(rnd(40)));
const ANSWERS = ['Train Times', '<think>hmm</think>Train Times', '<THINK>x', '<think>a</think>\n\n**Title: "Train times"**', '# Train times.', 'Title - Über Zug', 'chat title: “Hello”', '😀 Emoji title 😀', '<|im_end|>Name<|x|>', 'one two three four five six seven eight nine ten eleven', 'x'.repeat(70), 'word '.repeat(9) + 'x'.repeat(40), '', '   \n  ', '123', '...', null, 7, 'Kelvin <thinK>', 'a\r\nb', '«Quoted»', '`code`', 'Title:Plain'];
const answers = [...ANSWERS];
for (let i = 0; i < 600; i++) answers.push(pick(['', '<think>' + sentence(3) + '</think>', '**', '"', '# ']) + sentence(1 + rnd(14)) + pick(['', '.', '!!', ' ', '\n' + sentence(3)]));
(async () => {
  const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim() };
  out.derived = messages.map((m) => ({ m, out: t.derivedTitle(m) }));
  out.isDerived = [];
  for (const m of messages.slice(0, 300)) for (const title of [undefined, null, '', 5, t.derivedTitle(m), String(m ?? '').slice(0, 80), String(m ?? '').trim(), 'Other']) out.isDerived.push({ title: title === undefined ? '__undefined' : title, m: m === null ? '' : String(m ?? ''), out: t.isDerivedTitle(title, String(m ?? '')) });
  out.clean = answers.map((a) => ({ a, out: t.cleanTitle(a) }));
  out.requests = [];
  for (let i = 0; i < 200; i++) {
    const model = pick(['', 'm-synthetic', 'gemma']), message = pick(messages), reply = pick(answers);
    let sent = null;
    const fetch = async (url, init) => { sent = init.body; return new Response(JSON.stringify({ choices: [{ message: { content: 'Train times' } }] }), { status: 200 }); };
    const got = await t.generateTitle({ fetch, url: 'http://engine.invalid/v1/chat/completions', model, message: message ?? '', reply: reply ?? '' });
    out.requests.push({ model, message: String(message ?? ''), reply: String(reply ?? ''), body: sent, out: got });
  }
  const H = [[], [{ role: 'assistant', content: 'x' }], [{ role: 'user', content: 'first' }, { role: 'user', content: 'second' }], [null, { role: 'user', content: 7 }, { role: 'user', content: 'u' }], 'nope'];
  out.first = H.flatMap((h) => ['now', undefined, 5].map((m) => ({ h, m: m === undefined ? '__undefined' : m, reply: t.firstReply(h), user: t.firstUserMessage(h, m) })));
  out.system = t.TITLE_SYSTEM;
  fs.writeFileSync(process.argv[3], JSON.stringify(out));
  console.log(`titles: ${out.clean.filter((c) => c.out).length}/${out.clean.length} usable`);
})();
