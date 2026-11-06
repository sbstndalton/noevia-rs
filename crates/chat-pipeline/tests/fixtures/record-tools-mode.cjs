#!/usr/bin/env node
// Test-oracle glue only: calls noevia-core's own server/tools-mode.cjs (Automatic or Manual tools, what Automatic adds
// for one message, the carried public boxes, the tool-capable route, the ready checker) on synthetic messages,
// projects, boxes and histories, and records each answer. Needs dav-parse.wasm (prefetchableSearch): DAV_PARSE_WASM.
//   node record-tools-mode.cjs <noevia-core checkout> <output.json>
'use strict';
const path = require('node:path'), fs = require('node:fs');
const core = path.resolve(process.argv[2]);
const tm = require(path.join(core, 'server', 'tools-mode.cjs'));
let seed = 41;
const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return (seed >>> 16) % n; };
const pick = (xs) => xs[rnd(xs.length)];
const FRAGS = ['what is the latest', 'newest', 'recent news', 'most recently released versions', 'current stable version of node', 'currently prices',
  "today's weather", 'tonight\'s top headlines', 'this week s scores', 'news', 'stock prices', 'price of gold', 'cite your sources', 'with sources', 'source urls',
  'look up', 'lookup', 'search the web', 'search on the web for', 'web search', 'websearch', 'on the internet', 'from the web', 'search online', 'look it up online',
  'find it online', 'google it', 'google for cats', 'google drive', 'google docs for', 'google  this', 'browse the internet', 'browse web',
  'my notes', 'our shared team notes', 'my big old shared notes', 'nextcloud', 'Next cloud', 'NextCloud notes', 'my calendar', 'meetings', 'my agenda', 'schedule',
  'my to-dos', 'my todo', 'my reminders', 'tasks', 'my contacts', 'address book', 'my address  books', 'my e-mails', 'my inbox', 'mail', 'my files', 'my folders', 'our directories',
  'documents', 'docs', 'wikipedia', 'Wikipedia article', 'https://example.org/a', 'see http://x.invalid/p?q=1)', 'hello', 'how are you', 'write a poem about today\'s sunset',
  'the current through a resistor', 'latestish', 'mynotes', 'my notes', 'my\u0085notes', 'my﻿notes', 'looK up', 'neſws', 'NEWſ', 'my Kalendar',
  'noteſ', 'maİl', '```code```', '~~~', '> quoted', '\ud800 lone', 'emoji 😀 news', '\n', 'line\r\nbreak', '   ', 'latest news', 'my_notes', 'my-x notes', 'our a b c notes'];
const message = () => {
  if (rnd(30) === 0) return pick([undefined, null, 42, '', ' ', { a: 1 }]);
  const n = 1 + rnd(4); const parts = [];
  for (let i = 0; i < n; i++) parts.push(pick(FRAGS));
  let s = parts.join(pick([' ', ', ', '. ', '\n', '']));
  if (rnd(25) === 0) s = 'x'.repeat(3990 + rnd(20)) + ' latest news';
  if (rnd(25) === 0) s = 'y '.repeat(200) + s;
  return s;
};
const T = (name) => ({ type: 'function', function: { name } });
const BOXES = [
  { id: 'core', tools: [T('get_time'), T('read_project_file')] },
  { id: 'web-search', tools: [T('web_search')] },
  { id: 'offline-wikipedia', tools: [T('wiki_search')] },
  { id: 'web-crawl', tools: [T('fetch_url')] },
  { id: 'nextcloud-notes', server: 'nextcloud', tools: [T('nc_notes_list'), T('nc_notes_write')] },
  { id: 'nextcloud-calendar', server: 'nextcloud', tools: [T('nc_cal_write')] },
  { id: 'nextcloud-tasks', server: 'nextcloud', tools: [T('nc_tasks_list'), null, { function: null }, { function: { name: 7 } }] },
  { id: 'nextcloud-contacts', server: 'nextcloud', tools: [T('nc_contacts_list')] },
  { id: 'nextcloud-mail', server: 'nextcloud', tools: [T('nc_mail_send')] },
  { id: 'nextcloud-files', server: 'nextcloud', tools: [T('nc_files_list'), T('web_search')] },
  { id: 'gdrive', server: 'drive', tools: [T('drive_list')] },
  { id: 'mixed', tools: [T('wiki_search'), T('drive_list')] },
  { id: 'empty' }, { id: 'nulltools', tools: null }, { id: 7, tools: [T('x')] }, null,
  { id: 'web-search', tools: [T('web_search2')] },
];
const subset = (xs) => xs.filter(() => rnd(3) > 0);
const boxSubset = () => { const ix = BOXES.map((_, k) => k).filter(() => rnd(3) > 0); return { ix, boxes: ix.map((k) => BOXES[k]) }; };
const isWriteTool = (n) => /_(write|send)$/.test(n);
const out = { BOXES, modes: [], gates: [], boxesFor: [], additions: [], carried: [], order: [], ready: [], route: [] };
for (let i = 0; i < 2500; i++) { const m = message(); out.boxesFor.push({ message: m, wantsWeb: tm.wantsWeb(m), boxes: tm.automaticBoxesFor(m) }); }
const PROJ = [null, undefined, 0, 'x', {}, { toolsMode: 'auto' }, { toolsMode: 'manual' }, { toolsMode: 'other', toolboxes: [] }, { toolboxes: [] }, { toolboxes: {} }, { toolsMode: 'auto', toolboxes: ['a'] }];
for (const p of PROJ) out.modes.push({ project: p === undefined ? '__undef' : p, mode: tm.toolsModeOf(p), initial: tm.initialToolsMode(p) });
for (const g of [null, {}, { decision: 'none' }, { decision: '' }, { decision: { tool: 'x', mode: 'require' } }, { decision: 'x' }, { decision: 0 }]) out.gates.push({ gate: g, needed: tm.gateSaysToolNeeded(g) });
const IDS = ['core', 'web-search', 'offline-wikipedia', 'web-crawl', 'nextcloud-notes', 'nextcloud-files', 'nextcloud-mail', 'gdrive', 'mixed', 'empty', 'nulltools', 'project-docs'];
for (let i = 0; i < 2500; i++) {
  const m = rnd(2) ? message() : pick(['my notes and the latest news', 'search the web for my notes', 'wikipedia latest', 'nextcloud files', 'my calendar', 'news', 'google it', 'hi', 'my inbox and tasks', 'look up https://a.invalid/x']), base = subset(IDS).concat(rnd(10) === 0 ? [5] : []), { ix, boxes } = boxSubset(), connectors = subset(['gdrive', 'web-search', 'nextcloud-notes']);
  const notReady = subset(IDS).filter(() => rnd(3) === 0), throwing = rnd(8) === 0 ? pick(IDS) : null;
  const privateData = rnd(2) === 0, max = pick([3, 3, 1, 0, 5]), carried = rnd(3) === 0 ? subset(['web-search', 'offline-wikipedia', 'web-crawl', 'core', 4]) : pick([[], undefined, 'x']);
  const ready = (b) => { if (b.id === throwing) throw new Error('storage'); return notReady.includes(b.id) ? false : pick([true, true, 1]); };
  // ready's answers are drawn as it is called, so the Rust side replays them in call order.
  const answers = [];
  const rec = (b) => { try { const r = ready(b); answers.push(r === true); return r; } catch (e) { answers.push(false); throw e; } };
  let result, threw = false;
  try { result = tm.automaticAdditions({ message: m, base, boxes, connectorBoxes: new Set(connectors), isWriteTool, ready: rec, privateData, max, carried }); } catch { threw = true; }
  out.additions.push({ message: m, base, boxes: ix, connectors, privateData, max, carried: carried === undefined ? '__undef' : carried, answers, result: threw ? '__threw' : result });
}
const HIST = [{ role: 'user', content: 'search the web for cats' }, { role: 'user', content: 'hi' }, { role: 'user', content: 'my notes please' }, { role: 'user', content: 'wikipedia on x' },
  { role: 'assistant', content: 'latest news' }, { role: 'tool', name: 'web_search' }, { role: 'function', name: 'wiki_search' }, { role: 'tool', name: 'nc_notes_list' }, { role: 'tool', name: 'drive_list' },
  { role: 'tool', name: 'read_project_file' }, { role: 'tool' }, { role: 'user', content: 5 }, null, 'x', [1], { role: 'user', content: 'news' }, { role: 'tool', name: 'fetch_url' }];
for (let i = 0; i < 1500; i++) {
  const n = rnd(8), history = []; for (let k = 0; k < n; k++) history.push(pick(HIST));
  const { ix, boxes } = boxSubset();
  out.carried.push({ history, boxes: ix, result: tm.carriedPublicBoxes({ history, boxes }) });
}
for (let i = 0; i < 800; i++) {
  const ids = subset(IDS), added = subset(IDS), head = subset(IDS);
  out.order.push({ ids, added, head, result: tm.orderWithAdditions(ids, added, head) });
}
const STORAGES = [null, { kind: 'nextcloud', username: 'u', secret: 's', baseUrl: 'https://nc.invalid' }, { kind: 'webdav', username: 'u', secret: 's', baseUrl: 'https://bad.invalid' },
  { kind: 'gdrive', username: 'u', secret: 's' }, { kind: 'nextcloud', username: '', secret: 's' }, { kind: 'nextcloud', username: 'u' }, '__throw'];
for (let i = 0; i < 600; i++) {
  const user = pick([null, { id: 'u1' }, { id: 5 }, {}]), storage = pick(STORAGES), originThrows = rnd(6) === 0;
  let reads = 0;
  const ready = tm.createAutomaticToolsReady({ getStorage: () => { reads++; if (storage === '__throw') throw new Error('x'); return storage; },
    originAllowed: (u) => { if (originThrows) throw new Error('y'); return u === 'https://nc.invalid' ? true : pick([false, 1]); } })(user);
  const asks = []; for (let k = 0; k < 1 + rnd(4); k++) { const b = pick([null, { server: 'nextcloud' }, { server: 'drive' }, {}]); asks.push({ box: b, ready: ready(b) }); }
  out.ready.push({ user, storage, originThrows, asks, reads });
}
(async () => {
  const MODELS = ['m-a', 'm-b', 'm-c', ''];
  for (let i = 0; i < 1200; i++) {
    const model = pick(MODELS.slice(0, 3)), routedRole = pick([null, 'fast', 'smart', 'code', 'creative']);
    const roles = {}; for (const r of ['smart', 'code', 'fast']) if (rnd(4)) roles[r] = pick(MODELS);
    const capable = subset(['m-a@fast', 'm-a@smart', 'm-b@smart', 'm-b@code', 'm-c@fast', 'm-c@code', 'm-a@null', 'm-b@null', 'm-c@creative']);
    const hosts = {}; for (const r of ['fast', 'smart', 'code', 'creative', 'null']) hosts[r] = pick(['local', 'local', 'machine:1']);
    const rejectAt = rnd(8) === 0 ? rnd(3) : -1; const calls = [];
    const allowsTools = async (m, r) => { calls.push([m, r]); if (calls.length - 1 === rejectAt) throw new Error('probe'); return capable.includes(`${m}@${r}`); };
    let result; try { result = await tm.toolCapableRoute({ model, routedRole, roles, allowsTools, hostOf: (r) => hosts[String(r)] }); } catch { result = '__threw'; }
    out.route.push({ model, routedRole, roles, capable, hosts, rejectAt, calls, result });
  }
  fs.writeFileSync(process.argv[3], JSON.stringify(out));
  console.log(Object.entries(out).map(([k, v]) => `${k} ${v.length}`).join(', '));
})();
