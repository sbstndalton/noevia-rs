#!/usr/bin/env node
// Test-oracle glue only: asks noevia-core's own announced-action.cjs and final-answer.cjs about synthetic reply texts.
//   node record-rounds.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const announced = require(path.join(core, 'server', 'announced-action.cjs'));
const final = require(path.join(core, 'server', 'final-answer.cjs'));
let seed = 11;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const LEADS = ['I will', "I'll", 'I’ll', 'I am going to', "I'm going to", 'I’m going to', 'I need to', 'I have to', 'I should', 'Let me', "Let's", 'First,', 'First I', 'Then I will', 'Next, I’ll', 'i WILL', 'LET ME', 'You will', 'He will', 'Iwill', 'ſhould', 'I will', 'I\u0085will', 'I﻿will'];
const MIDS = ['', 'now ', 'first ', 'quickly ', 'carefully ', 'go ', 'go and ', 'Now ', 'really '];
const VERBS = ['read', 'open', 'check', 'look', 'look at', 'look up', 'look into', 'look through', 'search', 'list', 'fetch', 'load', 'scan', 'inspect', 'examine', 'retrieve', 'browse', 'query', 'calculate', 'count', 'compute', 'go through', 'use the', 'readme', 'reading', 'checK', 'checK', 'look  at', 'summarize'];
const ENDS = ['', ' the file.', ' it.', '.', ' the CSV and total it.', '!', '?', ':', ' everything', 'ı'];
const AFTER = ['', ' The total is 4.', ' Then: 5.', '\n\nDone.', ' and then answer'];
const PRE = ['', 'Sure. ', 'Okay!\n', 'x'.repeat(300) + '. ', '😀'.repeat(141), '(', 'Hmm;', 'No. ', ' \t '];
const texts = ['', '   ', 'I read the CSV; the total is 4', 'I will read the file', 7, null];
for (let i = 0; i < 3000; i++) texts.push(pick(PRE) + pick(LEADS) + ' ' + pick(MIDS) + pick(VERBS) + pick(ENDS) + pick(AFTER));
const URLS = ['https://a.example/x', 'http://b.example/y?q=1.', 'HTTPS://C.example/z', 'see https://a.example/x, and https://a.example/x!', 'xhttps://no.example', 'https://' + 'p'.repeat(420), '(https://d.example/(a))', 'https://e.example/"q"', 'ftp://f.example', 'https://g.example/é😀', 'https://h.example/a b', 'httpſ://i.example', 'https://Kelvin.example', '<https://j.example>', 'https://k.example/.,;:!?', '`https://l.example`'];
const links = [];
for (let i = 0; i < 400; i++) links.push(Array.from({ length: rnd(8) }, () => pick(URLS)).join(pick([' ', '\n', ' and ', ''])));
const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(),
  announced: texts.map((t) => ({ t, out: announced.announcesToolAction(t) })),
  links: links.map((t) => ({ t, out: final.sourceLinks(t) })),
  pass: [0, 1, 2, 3].flatMap((round) => [0, 1, 3].flatMap((toolCalls) => [false, true].map((hasContent) => ({ round, toolCalls, hasContent, out: final.needsFinalPass({ round, toolCalls, hasContent }) })))),
  fallback: [[], ['https://a'], ['1', '2', '3', '4', '5', '6']].map((sources) => ({ sources, out: final.fallbackText({ sources }) })),
  notes: { announced: announced.ANNOUNCED_NOTE, final: final.FINAL_ANSWER_NOTE } };
fs.writeFileSync(process.argv[3], JSON.stringify(out));
console.log(`announced true ${out.announced.filter((a) => a.out).length}/${out.announced.length}`);
