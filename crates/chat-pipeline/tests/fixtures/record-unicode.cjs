#!/usr/bin/env node
// Test-oracle glue only: asks noevia-core's own search-query.cjs, announced-action.cjs, final-answer.cjs and
// chat-title.cjs about synthetic texts that hold U+FFFF/U+FFFE beside the Kelvin sign and the long s
// (review S5–S9 P2: an in-band sentinel must not rewrite an original character).
//   node record-unicode.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs'), cp = require('node:child_process');
const core = path.resolve(process.argv[2]);
const sq = require(path.join(core, 'server', 'search-query.cjs'));
const announced = require(path.join(core, 'server', 'announced-action.cjs'));
const final = require(path.join(core, 'server', 'final-answer.cjs'));
const title = require(path.join(core, 'server', 'chat-title.cjs'));
const ODD = ['￿', '￾', 'K', 'ſ', '￿K', 'ſ￾', 'K', 's'];
const queries = [];
for (const o of ODD) {
  queries.push(`abc${o}def`, `${o}earch the web for cats`, `Search the web for ${o}elvin`, `Search the web for ${o}`,
    `look${o}up the web: x y z`, `cats ${o}. Cite the sources`, `cats, please cite${o} the sources`, `${o}${o}${o}`, `ab${o}`,
    `Please search the internet about ${o}cats and cite your links for ${o}.`);
}
const texts = [];
for (const o of ODD) texts.push(`I will ${o}earch it`, `I will chec${o} the file.`, `Let me ${o}can ${o}`, `Ok. I will read ${o}￿`, `I${o} will read`);
const links = [];
for (const o of ODD) links.push(`https://a.example/${o}x`, `http${o}://b.example`, `see https://c.example/p${o}q, https://c.example/p${o}q.`, `https://d.example/${o}￿Kſ￾`);
const answers = [];
for (const o of ODD) answers.push(`<thin${o}>x</think>Train ${o}`, `<think>a${o}</think>Title: ${o}Name`, `Name ${o}￿`);
const out = { core: cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(),
  queries: queries.map((t) => ({ t, out: sq.cleanSearchQuery(t), args: sq.cleanSearchArgs('web_search', JSON.stringify({ query: t, n: 1 })) })),
  announced: texts.map((t) => ({ t, out: announced.announcesToolAction(t) })),
  links: links.map((t) => ({ t, out: final.sourceLinks(t) })),
  titles: answers.map((t) => ({ t, out: title.cleanTitle(t) ?? null })) };
fs.writeFileSync(process.argv[3], JSON.stringify(out));
console.log(`queries ${out.queries.length} announced ${out.announced.length} links ${out.links.length} titles ${out.titles.length}`);
