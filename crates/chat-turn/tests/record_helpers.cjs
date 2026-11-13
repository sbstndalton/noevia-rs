#!/usr/bin/env node
'use strict';
// Recorded chat-turn helpers for crates/chat-turn: the contract src/helpers/ is replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout with server/wasm/dav-parse.wasm> \
//     node crates/chat-turn/tests/record_helpers.cjs > crates/chat-turn/tests/fixtures/node-helpers.v1.json
//   ... node crates/chat-turn/tests/record_helpers.cjs --check crates/chat-turn/tests/fixtures/node-helpers.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every value is computed by noevia-core's own functions over synthetic, QA-named inputs:
//   server/search-query.cjs     cleanSearchQuery, cleanSearchArgs
//   server/announced-action.cjs announcesToolAction (and ANNOUNCED_NOTE, TAIL_CHARS)
//   server/final-answer.cjs     needsFinalPass, sourceLinks, fallbackText (and the constants)
//   server/tool-route-notes.cjs toolRouteNotes
// The inputs are fixed lists crossed with each other plus seeded pseudo-random combinations (mulberry32, fixed seeds),
// so a recording is a pure function of the four files. Lone surrogates are written as JSON.stringify writes them
// (\udxxx escapes); the Rust replay reads them with js_json::wtf16. The source files' sha256 are recorded.
//
// toolRouteNotes with Tools: Automatic asks server/tools-mode.cjs automaticBoxesFor whether to add Web search. The Rust
// port takes that answer as an input, so every case records it (`automaticWeb`) from the real tools-mode.cjs.
// cleanSearchArgs cases marked `difference` are the named difference of the port (src/helpers/mod.rs): arguments
// js_json cannot hold are returned unchanged by Rust; Node's own output is still what is recorded.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const core = path.resolve(checkout);
const sha = (rel) => crypto.createHash('sha256').update(fs.readFileSync(path.join(core, rel))).digest('hex');
const SOURCES = {};
for (const rel of ['server/search-query.cjs', 'server/announced-action.cjs', 'server/final-answer.cjs', 'server/tool-route-notes.cjs']) SOURCES[rel] = sha(rel);
const AUTOMATIC_WEB_FROM = {};
for (const rel of ['server/tools-mode.cjs', 'server/tool-gate.cjs']) AUTOMATIC_WEB_FROM[rel] = sha(rel);
for (const level of ['log', 'info', 'warn', 'error']) console[level] = (...args) => process.stderr.write(`${args.map(String).join(' ')}\n`);

const { cleanSearchQuery, cleanSearchArgs } = require(path.join(core, 'server/search-query.cjs'));
const announced = require(path.join(core, 'server/announced-action.cjs'));
const finalAnswer = require(path.join(core, 'server/final-answer.cjs'));
const { toolRouteNotes } = require(path.join(core, 'server/tool-route-notes.cjs'));
const { automaticBoxesFor } = require(path.join(core, 'server/tools-mode.cjs'));
// tool-gate.cjs prefetchableSearch answers false when dav-parse.wasm does not load, which would silently change
// automaticWeb; refuse to record without it.
try {
  const probe = require(path.join(core, 'server/dav-parse-wasm.cjs')).toolGateQueries(['QA probe']);
  if (!Array.isArray(probe?.prefetchable) || typeof probe.prefetchable[0] !== 'boolean') throw new Error('no prefetchable answer');
} catch (error) {
  process.stderr.write(`dav-parse.wasm did not load (build server/wasm/dav-parse.wasm in the checkout): ${error?.message || error}\n`);
  process.exit(2);
}

// ---- determinism -----------------------------------------------------------------------------------------------
function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const pick = (rand, list) => list[Math.floor(rand() * list.length)];
const caseVariants = (rand, s) => [s, s.toUpperCase(), s.toLowerCase(),
  [...s].map((c) => (rand() < 0.5 ? c.toUpperCase() : c.toLowerCase())).join('')];
const uniq = (list) => [...new Set(list)];

// The units /\s/ matches, as ranges: the Rust port's is_js_space is checked against them.
const space = [];
for (let u = 0, start = -1; u <= 0x10000; u += 1) {
  const hit = u < 0x10000 && /\s/.test(String.fromCharCode(u));
  if (hit && start < 0) start = u;
  if (!hit && start >= 0) { space.push([start, u - 1]); start = -1; }
}

// Spaces JS counts (\s and trim) and lookalikes it does not (U+200B, U+180E, U+0085, U+2060).
const SPACES = [' ', '\t', '\n', '\r', '\v', '\f', '\u00a0', '\u1680', '\u2003', '\u2028', '\u2029', '\u202f', '\u205f', '\u3000', '\ufeff'];
const LOOKALIKES = ['\u200b', '\u180e', '\u0085', '\u2060'];

// ---- cleanSearchQuery ------------------------------------------------------------------------------------------
const WRAPPERS = ['', 'Search the web: ', 'search the web for ', 'Please search the internet for ', 'please  search\tthe web about ',
  'Look up online: ', 'lookup the web, ', 'look up the internet - ', 'Google the web: ', 'google online about ', 'Web search: ',
  'Internet search, ', 'web search- ', 'WEB SEARCH:', 'Search web ', 'search online', 'Search the net: ', 'Search the webs: ',
  'Searching the web: ', 'Web search ', ': ', '"', '“', '— ', '–', '`', "'", '...', ' , ', ';:', '‘’', 'Cite the sources: ',
  'cite your links - ', 'Cite urls ', 'cite a reference: ', 'Cite sources: Search the web: ', 'Search the web: Cite the source: ',
  ': "Search the web: ', 'Ｓearch the web: ', 'ſearch the web: ', 'Search\u00a0the\u00a0web: ', 'Search\ufeffthe web: ',
  'Search\u200bthe web: ', 'Search\u180ethe web: ', 'Search\u2028the web: ', 'Search\u3000the web: ', 'Search\u0085the web: ',
  'Search the web:: ', 'Search the web -', 'search the internet about: ', 'Cite: ', 'cite the sourcesQA '];
const QUESTIONS = ['what is the latest QA widget release?', 'QA weather in Testville today', 'how tall is the QA tower', 'qa', 'ab', 'x', '',
  'QA citation styles', 'how to cite sources in APA for the QA report', 'QA: the latest results', 'QA émigré café naïve',
  'QA 😀 emoji question', 'QA test 𝒳 astral', 'QA \ud83d lone high', 'QA lone low \ude00', 'K kelvin QA', 'the QA web search tool',
  'QA. Next sentence?', 'QA and cite', 'QA please', '-- QA dashes --', '“QA quoted”'];
const TAILS = ['', '?', ' Cite the source.', ' cite your sources for each claim', ' and cite your sources', ' Please cite the links.',
  '. Cite all references with links!', ' — cite sources', ' Cite sources: ', ' cite sourcesfor', ' cite the sources in the answer.  ',
  ' recite sources', ' and please cite urls to the reader, please.', ' cite the citations from QA. Then more', '?!', ' ;', ' :',
  ' CITE THE SOURCES', ' Cite\u00a0sources', ' cite\ufeffsources', ' Cite the sources\u3000', ' cite\u200bsources',
  ' and cite sources from https://qa.example.invalid/a?b=1', ' - cite a link', ',cite any url', ' cite references to QA! and more.',
  ' cite sources 😀', ' cite sources for 😀', ' andcite sources', ' pleasecite sources', ' cite your  sources ,.;'];
const PADS = ['', ' ', '\t\n', ...SPACES.slice(6), ...LOOKALIKES];
const queries = [];
for (const w of WRAPPERS) for (const t of TAILS) queries.push(`${w}what is the latest QA release${t}`);
for (const q of QUESTIONS) for (const w of WRAPPERS.slice(0, 8)) queries.push(`${w}${q}`);
for (const q of QUESTIONS) for (const t of TAILS.slice(0, 10)) queries.push(`${q}${t}`);
for (const p of PADS) queries.push(`${p}Search the web: QA padded${p}`, `${p}QA${p}`, `${p}ab${p}`, `${p}${p}`, `Search${p}the${p}web${p}QA spaced`);
{
  const rand = mulberry32(0x1523);
  for (let i = 0; i < 700; i += 1) {
    const raw = `${pick(rand, PADS)}${pick(rand, WRAPPERS)}${pick(rand, WRAPPERS)}${pick(rand, QUESTIONS)}${pick(rand, TAILS)}${pick(rand, TAILS)}${pick(rand, PADS)}`;
    queries.push(pick(rand, caseVariants(rand, raw)));
  }
}
const cleanQuery = uniq(queries).map((q) => [q, cleanSearchQuery(q)]);

// ---- cleanSearchArgs -------------------------------------------------------------------------------------------
const NAMES = ['web_search', 'search', 'SEARCH_web', 'Search', 'searx_query', 'drive_search_files', 'websearch', 'research_notes',
  'read_file', '', 'ſearch', 'ｓearch', 'nc_webdav_search', 'sea rch', 'SeArCh'];
const deep = (n, inner) => `${'['.repeat(n)}${inner}${']'.repeat(n)}`;
const ARGS = [
  ['plain', JSON.stringify({ query: 'Search the web: what is the latest QA release? Cite the source.' })],
  ['unchanged', JSON.stringify({ query: 'what is the latest QA release' })],
  ['keyOrder', '{"limit":3,"query":"Search the web: QA order","lang":"en"}'],
  ['keyOrderFirst', '{"query":"Search the web: QA first","limit":3}'],
  ['indexKeys', '{"b":1,"2":"x","query":"Search the web: QA index keys","1":"y","01":"z"}'],
  ['duplicate', '{"query":"Search the web: QA first dup","query":"Search the web: QA second dup"}'],
  ['duplicateLater', '{"query":"QA keep","n":1,"query":"Search the web: QA later wins"}'],
  ['duplicateToNumber', '{"query":"Search the web: QA","query":7}'],
  ['string', '"Search the web: QA string"'], ['array', '["Search the web: QA"]'], ['arrayOfObject', '[{"query":"Search the web: QA"}]'],
  ['null', 'null'], ['number', '42'], ['true', 'true'], ['queryNumber', '{"query":42}'], ['queryNull', '{"query":null}'],
  ['queryArray', '{"query":["Search the web: QA"]}'], ['queryObject', '{"query":{"q":"Search the web: QA"}}'], ['noQuery', '{"q":"Search the web: QA"}'],
  ['empty', '{}'], ['emptyText', ''], ['brace', '{'], ['notJson', 'not json'], ['trailingText', '{"query":"Search the web: QA"} trailing'],
  ['outerSpaces', '  {"query":"Search the web: QA ws"}\n\t'], ['bom', '\ufeff{"query":"Search the web: QA bom"}'],
  ['nbspOutside', '\u00a0{"query":"Search the web: QA nbsp"}'],
  ['escapedLone', '{"query":"Search the web: QA \\ud83d lone"}'], ['escapedPair', '{"query":"Search the web: QA \\ud83d\\ude00 pair"}'],
  ['rawLone', '{"query":"Search the web: QA \ud83d raw lone"}'], ['rawLoneKey', '{"k\udc00":1,"query":"Search the web: QA raw key"}'],
  ['rawLoneOutside', '{"query":"Search the web: QA"}\ud83d'], ['rawLoneAfterBackslash', '{"query":"Search the web: QA \\\ud83d"}'],
  ['rawLoneInHex', '{"query":"Search the web: QA \\u12\ud83d"}'], ['astral', '{"query":"Search the web: QA 😀 astral 𝒳"}'],
  ['numbers', '{"query":"Search the web: QA numbers","a":1.50,"b":-0,"c":1e21,"d":0.1,"e":1E-7,"f":123456789012345680000}'],
  ['bigNumber', '{"query":"Search the web: QA big","n":1e400}', 'unsupported'],
  ['bigNumberUnchanged', '{"query":"QA plain","n":1e400}'],
  ['nested', `{"query":"Search the web: QA nested","x":${deep(50, '1')}}`],
  ['tooDeep', `{"query":"Search the web: QA deep","x":${deep(200, '1')}}`, 'unsupported'],
  ['stackDeep', `{"query":"Search the web: QA stack","x":${deep(10000, '1')}}`, 'throws'],
  ['proto', '{"__proto__":{"a":1},"query":"Search the web: QA proto"}'],
  ['escapes', '{"query":"Search the web: QA \\u0041 \\n \\"quote\\" \\/ slash \\\\ back"}'],
  ['lineSeparators', '{"query":"Search the web: QA \u2028\u2029 sep","s":"</script>"}'],
  ['controlEscapes', '{"query":"Search the web: QA \\u0001\\u001f\\u007f\\u00ad"}'],
  ['onlyTrim', '{"query":"  QA trimmed  "}'], ['tooShort', '{"query":"Search the web: ab"}'], ['tooShortTrim', '{"query":" ab "}'],
  ['spaced', '{ "query" : "Search the web: QA spaced" , "x" : [ 1 , 2 ] , "y" : { } }'],
  ['nestedQuery', '{"opts":{"query":"Search the web: inner"},"query":"Search the web: QA outer"}'],
  ['rawControl', '{"query":"Search the web: QA \u0001 raw control"}'], ['upperKey', '{"Query":"Search the web: QA"}'],
];
const cleanArgs = [];
for (const name of NAMES) for (const [label, raw, difference] of ARGS.slice(0, 4)) cleanArgs.push({ label, name, raw, difference });
for (const [label, raw, difference] of ARGS) cleanArgs.push({ label, name: 'web_search', raw, difference });
{
  const rand = mulberry32(0x2026);
  for (let i = 0; i < 160; i += 1) {
    const query = pick(rand, cleanQuery)[0];
    const extra = rand() < 0.5 ? { limit: Math.floor(rand() * 10) } : { lang: 'qa', safe: rand() < 0.5 };
    const raw = rand() < 0.5 ? JSON.stringify({ query, ...extra }) : JSON.stringify({ ...extra, query });
    cleanArgs.push({ label: `generated${i}`, name: pick(rand, NAMES), raw });
  }
}
for (const c of cleanArgs) {
  try { c.out = cleanSearchArgs(c.name, c.raw); } catch (error) { c.throws = error.name; }
  if (c.difference === undefined) delete c.difference;
}

// ---- announcesToolAction ---------------------------------------------------------------------------------------
const ANNOUNCEMENTS = ['I will read the full file and total the months.', "I'll check the QA folder.", 'I’ll open it', 'I am going to search the web',
  "I'm going to look up the QA term", 'I’m going to look  into it', 'I need to fetch the data', 'I have to load the sheet.', 'I should inspect the log',
  'Let me check.', 'let me now read the file', "Let's search for it", 'First, I will read the notes.', "Then I'll compute the sum",
  'Next I’ll go through the list', 'next, I scan the doc', 'I will go and check', 'I will go check', 'I will quickly calculate',
  'I will carefully examine', 'I will use the tool', 'I will use a tool', 'I will reading', 'I will readily agree', 'I willread', 'Iwill read',
  "I'll\tcheck", 'I\u00a0will read', 'I\ufeffwill read', 'I\u200bwill read', 'I will read\u200b', 'I will retrieve the QA record', 'I will browse',
  'I will query the database', 'I will count rows', 'I will list files', 'I will look at it', 'I will look through', 'I will look', 'I will lookup',
  'Let me go through it', 'I should go through', 'We will read it', "you'll read", 'AI will read', '(I will read)', ';let me check', 'I WILL READ',
  'i will read', 'LET ME CHECK', 'İ will read', 'ı will read', 'I will first scan', 'I will now now read', 'I will go and  check', 'I will looks',
  'I will read_file', 'I will read-only', 'I will read2', 'Then, I will open', 'then,I will open', 'First,I read', 'I ’ll read', "I ' ll read",
  'I will\u2028read', 'Let\u3000me check', 'I will go  through', 'I will use  the', 'I will usethe', 'I will check…', 'let\'s browse', "LET'S LIST"];
const PREFIXES = ['', 'The total is 4.', 'I read the CSV; the total is 4.', 'Sure!', 'Here is the plan:', 'Done:\n', 'QA', 'QA 😀.', 'Answer: 4.',
  'The QA total is 4', 'Done!', 'Ok?', 'Note…'];
const SUFFIXES = ['', '\n\nThe answer is 4.', ' The answer is 4.', ': the file says 4', '.', ' ', '\u200b', '\ufeff', ' Then the answer.', '!',
  ' and total it', '?\u00a0', ':\nThe end', '. ', ' 😀'];
const SEPARATORS = ['', ' ', '\n', '\n\n', '\u00a0', '\u3000', '\u200b', '\t'];
const texts = [];
for (const a of ANNOUNCEMENTS) for (const p of ['', 'The total is 4. ', 'Done!']) texts.push(`${p}${a}`);
for (const a of ANNOUNCEMENTS.slice(0, 12)) for (const s of SUFFIXES) texts.push(`${a}${s}`);
for (const p of PREFIXES) for (const sep of SEPARATORS) texts.push(`${p}${sep}I will read it`, `${p}${sep}${p}`);
// The 280-unit tail cut: the announcement starts just inside or outside it, and an astral character straddles it.
const filler = 'QA filler words without a stop sign here ';
for (let n = 0; n < 24; n += 1) {
  const head = filler.repeat(8).slice(0, 255 + n);
  texts.push(`${head}. I will read the file`, `${head}. Let me check`, `${head}😀 I will read`, `${head}😀I will read`);
  texts.push(`${'😀'.repeat(140 + (n % 3))}${'x'.repeat(n % 2)} I will read`);
}
for (let n = 270; n < 290; n += 1) texts.push(`${'y'.repeat(n)}${'I will read'}`, `I will read. ${'z'.repeat(n)}`, `${'😀'.repeat(n / 2 | 0)}.\ud83d I will check`);
texts.push('', ' ', '\u3000\ufeff', '\u200b', '.', '. . .', '\ud83d', '\ude00 I will read', 'I will read \ud83d', 'I will read.\ud83d');
{
  const rand = mulberry32(0x1552);
  for (let i = 0; i < 900; i += 1) {
    const raw = `${pick(rand, PREFIXES)}${pick(rand, SEPARATORS)}${pick(rand, ANNOUNCEMENTS)}${pick(rand, SUFFIXES)}${rand() < 0.2 ? `${pick(rand, SEPARATORS)}${pick(rand, ANNOUNCEMENTS)}` : ''}`;
    texts.push(pick(rand, caseVariants(rand, raw)));
  }
}
const announces = uniq(texts).map((t) => [t, announced.announcesToolAction(t)]);

// ---- final answer ----------------------------------------------------------------------------------------------
const needsFinalPass = [];
for (let round = 0; round <= 5; round += 1) for (let toolCalls = 0; toolCalls <= 3; toolCalls += 1) for (const hasContent of [false, true]) {
  needsFinalPass.push([round, toolCalls, hasContent, finalAnswer.needsFinalPass({ round, toolCalls, hasContent })]);
}
const U = 'https://qa.example.invalid';
const LINK_TEXTS = [`See ${U}/a.`, 'HTTPS://QA.EXAMPLE.INVALID/B, and http://qa.example.invalid/c;', `x${U}/no`, `_${U}/no`, `-${U}/dash`, `(${U}/p)`,
  `<${U}/x>`, `"${U}/q"`, `'${U}/s'`, `\`${U}/t\``, `[${U}/u]`, `{${U}/v}`, `${U}/w?x=1&y=2#frag!?!.`, `${U}/${'a'.repeat(373)}`,
  `${U}/${'a'.repeat(374)}`, `${U}/${'a'.repeat(375)}`, `${U}/${'😀'.repeat(187)}`, `${U}/${'😀'.repeat(187)}a`, `${U}/😀.`, `${U}/\ud83d.`,
  `${U}/\ude00x`, `${U}/1 ${U}/1. ${U}/1, ${U}/1;:!?`, [1, 2, 3, 4, 5, 6, 7].map((n) => `${U}/${n}`).join(' '),
  [1, 1, 2, 2, 3, 4, 5, 6].map((n) => `${U}/${n}.`).join('\n'), 'ftp://qa.example.invalid/f', 'http:/qa.example.invalid', 'https://.',
  'https://...', 'https://?', `${U}/a\u00a0next`, `${U}/a\u200bnext`, `${U}/x\ufeffy`, 'HtTpS://mixed.example.invalid/M', 'httpss://qa.invalid',
  'https//qa.invalid', `${U}/${'b'.repeat(400)} ${U}/after-long`, `ERROR ${U}/e`, `${U}/a)b ${U}/c]d ${U}/e}f`, `${U}/q'r ${U}/s"t ${U}/u\`v`,
  `${U}/é ${U}/ü`, 'https://ſ.invalid', '', 'no links here', `${U}`, 'https://', `${U}/a,b,c,,`, `é${U}/after-letter`, `1${U}/after-digit`,
  `${U}/a\n${U}/b\r\n${U}/c\t${U}/d`, `a.https://qa.invalid/dot`, `${'h'.repeat(5)}ttps://qa.invalid`];
const linkTexts = [...LINK_TEXTS];
{
  const rand = mulberry32(0x1693);
  const words = ['QA', 'result', 'see', 'from', '(', ')', '.', ',', '\n', 'http', 'https:', '😀', '\u00a0', 'and', '[', '<'];
  const urls = [1, 2, 3, 4, 5, 6, 7, 8].map((n) => `${pick(rand, ['http', 'https', 'HTTP', 'Https'])}://qa${n}.example.invalid/${n}`);
  for (let i = 0; i < 300; i += 1) {
    const parts = [];
    const count = 1 + Math.floor(rand() * 14);
    for (let j = 0; j < count; j += 1) parts.push(rand() < 0.4 ? `${pick(rand, urls)}${pick(rand, ['', '.', ',', ';', '?!', ')', ':'])}` : pick(rand, words));
    linkTexts.push(parts.join(pick(rand, [' ', '', '\n'])));
  }
}
const sourceLinks = uniq(linkTexts).map((t) => [t, finalAnswer.sourceLinks(t)]);
const fallbackText = [];
for (let n = 0; n <= 7; n += 1) {
  const sources = Array.from({ length: n }, (_, i) => `${U}/source-${i + 1}`);
  fallbackText.push([sources, finalAnswer.fallbackText({ sources })]);
}
for (const sources of [[`${U}/😀`, `${U}/\ud83d`], ['', ' '], ['- already', 'a\nb']]) fallbackText.push([sources, finalAnswer.fallbackText({ sources })]);

// ---- toolRouteNotes --------------------------------------------------------------------------------------------
const MESSAGES = ['', 'QA hello there', 'List my Nextcloud top folders', 'nextcloudy QA', 'NEXTCLOUD QA notes', 'search google drive for QA plan',
  'Google\u00a0Drive QA', 'Google\u200bDrive QA', 'open my drive', 'the drive QA', 'thedrive QA', 'What is the latest QA release?',
  'QA weather today', 'up-to-date QA', 'up to date', 'upto date', 'right  now', 'right now', 'this week', 'this  week', 'as of 2026',
  'currently', 'recently', 'QA news', 'newsletter QA', 'exchange rate', 'stock price', 'pack QA-1786 list for camping', 'pack my QA files',
  'QA pdf', 'QA dav', 'webdav QA', 'Nextcloud and Google Drive today', 'my drive news', 'ſtock price', 'LATEST', 'Nextcloud_files',
  'my Drive', 'MY DRIVE', 'search the web for the latest QA', 'https://qa.example.invalid/page latest', 'Summarise https://qa.example.invalid/x',
  'what is on wikipedia about QA', 'my notes and my calendar today', 'QA uploaded spreadsheet', 'QA sheets', 'QA ticket QA-42', 'tonight QA',
  'Nextcloud\ufeffQA', 'QA 😀 Nextcloud 😀', '\ud83dNextcloud', 'newest drive file', 'QA docs on Google  Drive'];
const OFFERED = [[], ['nextcloud-files'], ['gdrive'], ['nextcloud-files', 'gdrive'], ['web-search'], ['gdrive', 'web-search'],
  ['nextcloud-files', 'gdrive', 'web-search'], ['core'], ['gdrive', 'nextcloud-files', 'gdrive']];
const ON_SERVER = [[], ['web-search'], ['web-search', 'nextcloud-files', 'gdrive'], ['core']];
const FLAGS = [];
for (const automatic of [undefined, false, true]) for (const localModel of [undefined, true, false]) for (const gateNone of [undefined, false, true]) FLAGS.push({ automatic, localModel, gateNone });
const SET_FLAGS = FLAGS.filter((f) => f.automatic !== undefined && f.localModel !== undefined && f.gateNone !== undefined);
const routeInputs = [];
for (const message of ['List my Nextcloud top folders', 'search google drive for QA plan', 'What is the latest QA release?', 'pack QA-1786 list for camping', 'Nextcloud and Google Drive today']) {
  for (const offered of OFFERED) for (const onServer of ON_SERVER) for (const f of SET_FLAGS) routeInputs.push({ message, offered, onServer, ...f });
}
{
  const rand = mulberry32(0x1520);
  for (const message of MESSAGES) for (let i = 0; i < 14; i += 1) routeInputs.push({ message, offered: pick(rand, OFFERED), onServer: pick(rand, ON_SERVER), ...pick(rand, FLAGS) });
}
const notes = [];
const noteIndex = new Map();
const routeNotes = routeInputs.map((input) => {
  const call = Object.fromEntries(Object.entries(input).filter(([, v]) => v !== undefined));
  const out = toolRouteNotes(call);
  if (!noteIndex.has(out)) { noteIndex.set(out, notes.length); notes.push(out); }
  return { ...call, automaticWeb: automaticBoxesFor(input.message).includes('web-search'), out: noteIndex.get(out) };
});

// ---- characters U+10F800..U+10FFFF: outside the wtf16 domain ----------------------------------------------------
// A Rust caller holds JS text in js_json::wtf16 form, where these code points stand for lone surrogates (U+10F800 is
// \ud800). Each input is recorded in that form (`input`, lone surrogates), with Node's result for it (`mapped`) and
// Node's result for the real characters (`real`, every string as its UTF-16 code units: wtf16 cannot carry them);
// the replay checks the Rust result is `mapped`.
const toMapped = (s) => s.replace(/[\u{10F800}-\u{10FFFF}]/gu, (c) => String.fromCharCode(0xd800 + c.codePointAt(0) - 0x10f800));
const REAL = ['Search the web: a\u{10F800}', 'Search the web: QA \u{10FFFF} x', '\u{10F800}', 'Search the web: \u{10F800}\u{10FBFF}'];
const asUnits = (v) => (typeof v === 'string' ? Array.from({ length: v.length }, (_, i) => v.charCodeAt(i)) : Array.isArray(v) ? v.map(asUnits) : v);
const outOfDomain = [];
for (const r of REAL) outOfDomain.push({ fn: 'cleanSearchQuery', input: toMapped(r), real: asUnits(cleanSearchQuery(r)), mapped: cleanSearchQuery(toMapped(r)) });
for (const r of [`I will read\u{10F800}`, `${'\u{10F800}'.repeat(140)} I will read`, `${'y'.repeat(270)}\u{10FFFF}I will read`]) {
  outOfDomain.push({ fn: 'announcesToolAction', input: toMapped(r), real: asUnits(announced.announcesToolAction(r)), mapped: announced.announcesToolAction(toMapped(r)) });
}
for (const r of [`${U}/${'\u{10FFFD}'.repeat(10)}`, `${U}/${'a'.repeat(250)}${'\u{10F800}'.repeat(150)}`]) {
  outOfDomain.push({ fn: 'sourceLinks', input: toMapped(r), real: asUnits(finalAnswer.sourceLinks(r)), mapped: finalAnswer.sourceLinks(toMapped(r)) });
}
for (const r of REAL) {
  const raw = JSON.stringify({ query: r });
  outOfDomain.push({ fn: 'cleanSearchArgs', input: toMapped(raw), real: asUnits(cleanSearchArgs('web_search', raw)), mapped: cleanSearchArgs('web_search', toMapped(raw)) });
}

// ---- seeded random text through every helper --------------------------------------------------------------------
const TOKENS = ['cite', 'Cite', 'CITE', 'sources', 'source', 'links', 'for', 'and', 'please', 'search', 'Search the web:', 'web search:',
  'google online', 'I', 'i', 'will', "I'll", 'I’ll', 'let me', "let's", 'read', 'check', 'look up', 'go through', 'first,', `${U}/r`,
  'http://QA.invalid/', 'HTTPS://', '.', '!', '?', ':', ';', ',', '-', '"', '“', '(', ')', ' ', '  ', '\n', '\t', '᠎', '﻿', 'K',
  'ſ', 'ı', 'İ', '\ud83d', '\ude00', '😀', ' ', '　', '​', ' ', 'QA', 'nextcloud', 'google drive', 'latest', 'files',
  'x'.repeat(40)];
const random = [];
{
  const rand = mulberry32(0x1e57);
  for (let i = 0; i < 500; i += 1) {
    const parts = [];
    if (rand() < 0.25) parts.push('y'.repeat(230 + Math.floor(rand() * 80)), pick(rand, ['😀', '\ud83d', '']));
    const count = 1 + Math.floor(rand() * 30);
    for (let j = 0; j < count; j += 1) parts.push(pick(rand, TOKENS), rand() < 0.6 ? ' ' : '');
    if (rand() < 0.3) parts.push(pick(rand, SEPARATORS), pick(rand, ANNOUNCEMENTS), pick(rand, SUFFIXES));
    const text = parts.join('');
    const noteInput = { message: text, offered: ['gdrive'], onServer: ['web-search'], automatic: rand() < 0.3, gateNone: true };
    const out = toolRouteNotes(noteInput);
    if (!noteIndex.has(out)) { noteIndex.set(out, notes.length); notes.push(out); }
    random.push({ text, query: cleanSearchQuery(text), announces: announced.announcesToolAction(text), links: finalAnswer.sourceLinks(text),
      args: cleanSearchArgs('web_search', JSON.stringify({ query: text, n: 1 })), automatic: noteInput.automatic,
      automaticWeb: automaticBoxesFor(text).includes('web-search'), notes: noteIndex.get(out) });
  }
}

// ---- inputs V8 handles in linear or bounded time, replayed under a time limit --------------------------------------
const hostile = ['web search: '.repeat(4000) + 'QA', `x${' '.repeat(2000)}cite sources for${' '.repeat(2000)}.x`,
  `${'Search the web: '.repeat(500)}${'cite sources '.repeat(300)}QA`, `QA ${'cite sources for '.repeat(400)}`]
  .map((q) => [q, cleanSearchQuery(q)]);

// ---- the recording ---------------------------------------------------------------------------------------------
const fixture = {
  version: 1,
  generator: 'crates/chat-turn/tests/record_helpers.cjs (noevia-core search-query, announced-action, final-answer, tool-route-notes)',
  sources: SOURCES,
  automaticWebFrom: AUTOMATIC_WEB_FROM,
  constants: { ANNOUNCED_NOTE: announced.ANNOUNCED_NOTE, TAIL_CHARS: announced.TAIL_CHARS, FINAL_ANSWER_NOTE: finalAnswer.FINAL_ANSWER_NOTE,
    LAST_TOOL_ROUND: finalAnswer.LAST_TOOL_ROUND, MAX_SOURCES: finalAnswer.MAX_SOURCES },
  space,
  cleanSearchQuery: cleanQuery,
  cleanSearchArgs: cleanArgs,
  announcesToolAction: announces,
  needsFinalPass,
  sourceLinks,
  fallbackText,
  toolRouteNotes: { notes, cases: routeNotes },
  outOfDomain,
  random,
  hostile,
};
const text = `${JSON.stringify(fixture, null, 1)}\n`;
const check = process.argv.indexOf('--check');
if (check > 0) {
  const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
  if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
  process.stderr.write(`ok: ${cleanQuery.length} queries, ${cleanArgs.length} args, ${announces.length} replies, ${sourceLinks.length} link texts, ${routeNotes.length} route cases, ${random.length} random texts recorded as committed\n`);
} else process.stdout.write(text);
setImmediate(() => process.exit(0));
