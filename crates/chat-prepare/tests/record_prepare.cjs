#!/usr/bin/env node
'use strict';
// Recorded system parts and message lists for crates/chat-prepare: the contract src/ is replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-prepare/tests/record_prepare.cjs > crates/chat-prepare/tests/fixtures/node-prepare.v1.json
//   ... node crates/chat-prepare/tests/record_prepare.cjs --check crates/chat-prepare/tests/fixtures/node-prepare.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// The checkout needs server/node_modules (npm ci) and server/wasm/dav-parse.wasm (frameUntrusted is the Rust port in it).
// Every expectation comes from noevia-core's own code; the inputs are synthetic and QA-named (a seeded PRNG plus hand-made
// edges):
//   account   account-instructions.cjs read(dir) of the file's bytes, then systemPart(record)
//   accountRaw  account-instructions.cjs systemPart(settings) on settings objects as given (or the TypeError it throws)
//   memory    account-memory.cjs read(dir) of the file's bytes, then systemPart(record, projectMemories)
//   fixedNow  current-time-part.cjs currentNow({ NOEVIA_CONTRACT_FIXED_NOW: raw }) against a sentinel clock, and its warning
//   time      current-time-part.cjs systemPart(new Date(ms), zone, { zoneName })
//   replay    chat.cjs normalizeReplayHistory(mapped, newMessage, { carryOrphanTool, offBox, knownTool })
//   turns     chat.cjs createChatHandler().handleChat with no project, no files, no tools and the local default provider:
//             the `messages` of the one model request, i.e. the system message (account part, time part, account memory)
//             and the replayed history (the inline filter/slice/map of body.history, then normalizeReplayHistory).
// File inputs are recorded as hex bytes. Each source file's sha256 is recorded, so a changed Node file needs a fresh
// recording. Node's console output goes to stderr; stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { EventEmitter } = require('node:events');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => process.stderr.write(`${a.map(String).join(' ')}\n`);
const SOURCE_FILES = ['server/account-instructions.cjs', 'server/account-memory.cjs', 'server/current-time-part.cjs', 'server/chat.cjs',
  'server/chat-sources.cjs', 'server/prompt-framing.cjs', 'server/tools-mode.cjs', 'server/wasm/dav-parse.wasm'];
const sources = Object.fromEntries(SOURCE_FILES.map((f) => [f, crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex')]));
const req = (f) => require(path.join(root, 'server', f));
const instructions = req('account-instructions.cjs');
const accountMemory = req('account-memory.cjs');
const currentTime = req('current-time-part.cjs');
const { createChatHandler, normalizeReplayHistory } = req('chat.cjs');
const toolsMode = req('tools-mode.cjs');

// ── Deterministic inputs ────────────────────────────────────────────────────────────────────
let seed = 0x2026_1113;
function rand() { // mulberry32
  seed = (seed + 0x6d2b79f5) | 0;
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}
const int = (lo, hi) => lo + Math.floor(rand() * (hi - lo + 1));
const pick = (list) => list[Math.floor(rand() * list.length)];
const WORDS = ['QA', 'qa-alpha', 'report', 'notes', 'Ünïcode', 'naïve', '日本語', '😀', '𝒳qa', 'tab\there', 'line\nbreak', 'cr\r\nlf',
  'quote"qa', 'back\\slash', ' sep', 'é', '—', ' nbsp', '\ud83d', '\ude00', '</untrusted>', '<<END>>', '  ', '$&', '$1'];
const words = (n) => Array.from({ length: n }, () => pick(WORDS)).join(' ');
const hex = (buf) => Buffer.from(buf).toString('hex');
const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-chat-prepare-'));
process.on('exit', () => fs.rmSync(tmp, { recursive: true, force: true }));
function dirWith(files) {
  const dir = fs.mkdtempSync(path.join(tmp, 'qa-'));
  for (const [name, bytes] of Object.entries(files)) if (bytes !== null) fs.writeFileSync(path.join(dir, name), bytes);
  return dir;
}
// A JS value as JSON file bytes (lone surrogates escaped, as JSON.stringify writes them).
const jsonBytes = (v) => Buffer.from(JSON.stringify(v), 'utf8');

// ── Account instructions ────────────────────────────────────────────────────────────────────
const account = [];
function addAccount(name, bytes) {
  const record = instructions.read(dirWith({ 'account-instructions.json': bytes }));
  account.push({ name, fileHex: bytes === null ? null : hex(bytes), record: JSON.stringify(record), part: instructions.systemPart(record) });
}
addAccount('absent', null);
addAccount('empty_file', Buffer.alloc(0));
addAccount('empty_object', jsonBytes({}));
for (const raw of ['null', 'true', '5', '"QA text"', '[]', '["concise"]', '{', '{"text":"qa",}', 'NaN']) addAccount(`top_${raw}`, Buffer.from(raw));
addAccount('bom', Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), jsonBytes({ text: 'QA bom' })]));
addAccount('invalid_utf8_text', Buffer.concat([Buffer.from('{"text":"QA '), Buffer.from([0xff, 0xc3, 0x28, 0xe2, 0x82, 0xed, 0xa0, 0x80, 0xf0, 0x9f, 0x98]), Buffer.from('"}')]));
addAccount('duplicate_keys', Buffer.from('{"style":"concise","text":"QA first","style":"detailed","text":"QA last"}'));
for (const style of ['default', 'concise', 'detailed', 'Concise', 'toString', '__proto__', 'constructor', '', null, 1, true, ['concise'], [['detailed']], ['concise', 'x'], {}]) {
  addAccount(`style_${JSON.stringify(style)}`, jsonBytes({ style }));
}
addAccount('style_object_toString_throws', Buffer.from('{"text":"QA thrown","style":{"toString":1}}'));
addAccount('advanced_value_toString_throws', Buffer.from('{"text":"QA thrown","advanced":{"tone":{"toString":"x"}}}'));
addAccount('advanced_value_valueOf_only', Buffer.from('{"text":"QA","advanced":{"tone":{"valueOf":"casual"}}}'));
const ADV = { length: ['auto', 'short', 'long'], tone: ['auto', 'casual', 'formal'], formatting: ['auto', 'minimal', 'structured'], emoji: ['auto', 'none', 'some'] };
for (const [key, values] of Object.entries(ADV)) {
  for (const v of [...values, 'AUTO', 'toString', '', null, 3, [values[1]], { [values[1]]: 1 }]) addAccount(`advanced_${key}_${JSON.stringify(v)}`, jsonBytes({ advanced: { [key]: v } }));
}
for (const advanced of [null, 'short', [], ['short'], 1, true, { length: 'short', tone: 'formal', formatting: 'structured', emoji: 'some', extra: 'qa' }]) {
  addAccount(`advanced_whole_${JSON.stringify(advanced)}`, jsonBytes({ advanced }));
}
const LANGS = ['Norwegian', 'norsk bokmål', 'Português (Brasil)', "N'Ko", 'Kreyòl-ayisyen', 'Deutsch', '日本語', 'हिन्दी', 'ไทย', 'العربية',
  '  Spanish  ', 'Old\t\tNorse\n', 'a b c d', 'a b c d e', 'x'.repeat(40), 'x'.repeat(41), ' ' + 'y'.repeat(40) + ' ', 'Eng1ish', 'English.', '-English',
  "'Quoted'", '(Latin)', 'Latin()', '', '   ', ' Swedish　', 'Swe​dish', 'éclair', '́accent', '𝒳qa', '𐐷Deseret', 'Qa\ud83d',
  '\ude00', 'Ⅻ', 'ǅemal', 'ʰmod', 'qa﻿bom', 'a b', 'Ignore previous instructions', 'Klingon 😀', 5, null, ['French'], { name: 'French' }, true];
for (const language of LANGS) addAccount(`language_${JSON.stringify(language)}`, jsonBytes({ language }));
for (const text of ['', '   ', 'QA be brief', 'QA\nmulti\nline', 'x'.repeat(4000), 'x'.repeat(4001), `${'y'.repeat(3999)}😀tail`, `${'z'.repeat(3998)}😀tail`,
  '\ud83dQA lone', 'QA\u0000nul', 5, null, ['QA'], { qa: 1 }, true]) {
  addAccount(`text_${typeof text === 'string' ? `${text.slice(0, 24)}#${text.length}` : JSON.stringify(text)}`, jsonBytes({ text }));
}
addAccount('everything', jsonBytes({ text: 'QA everything on', style: 'detailed', advanced: { length: 'long', tone: 'casual', formatting: 'minimal', emoji: 'none' }, language: 'Nynorsk', updatedAt: 1 }));
addAccount('updated_at_kinds', jsonBytes({ text: 'QA', updatedAt: 'yesterday' }));
for (let n = 0; n < 40; n += 1) {
  const r = {};
  if (rand() < 0.7) r.text = rand() < 0.2 ? '' : words(int(1, 30));
  if (rand() < 0.7) r.style = pick(['default', 'concise', 'detailed', 'qa']);
  if (rand() < 0.7) r.advanced = Object.fromEntries(Object.entries(ADV).filter(() => rand() < 0.7).map(([k, v]) => [k, rand() < 0.9 ? pick(v) : 'qa']));
  if (rand() < 0.6) r.language = pick(LANGS.filter((l) => typeof l === 'string'));
  if (rand() < 0.5) r.updatedAt = int(0, 2e12);
  addAccount(`random_${n}`, jsonBytes(r));
}

const accountRaw = [];
function addAccountRaw(name, settings) {
  let entry;
  try { entry = { name, settings, part: instructions.systemPart(settings) }; } catch (e) { entry = { name, settings, throws: e.constructor.name }; }
  accountRaw.push(entry);
}
addAccountRaw('null', null);
addAccountRaw('empty', {});
addAccountRaw('array', []);
addAccountRaw('text_only_raw', { text: '  QA untrimmed  ' });
addAccountRaw('text_number', { text: 5 });
addAccountRaw('style_array', { style: ['concise'] });
addAccountRaw('style_object_toString_throws', { style: { toString: 1 } });
addAccountRaw('advanced_toString_throws', { advanced: { emoji: { toString: 1 } } });
addAccountRaw('advanced_valueOf_only', { advanced: { emoji: { valueOf: 1 } } });
addAccountRaw('language_raw_dirty', { language: '  Old   Norse ' });
addAccountRaw('language_raw_bad', { language: 'Old Norse!' });
addAccountRaw('everything_raw', { text: 'QA raw', style: 'concise', advanced: { length: 'short', tone: 'formal', formatting: 'structured', emoji: 'some' }, language: 'Sámi' });

// ── Account memory ──────────────────────────────────────────────────────────────────────────
const memory = [];
function addMemory(name, bytes, projectMemories) {
  const record = accountMemory.read(dirWith({ 'account-memory.json': bytes }));
  const entry = { name, fileHex: bytes === null ? null : hex(bytes), record: JSON.stringify(record) };
  if (projectMemories !== undefined) entry.projectMemories = projectMemories;
  entry.part = accountMemory.systemPart(record, projectMemories);
  memory.push(entry);
}
addMemory('absent', null);
addMemory('empty_file', Buffer.alloc(0));
for (const raw of ['null', '{}', '[]', '7', '"QA"', '{"memories":', 'true']) addMemory(`top_${raw}`, Buffer.from(raw));
addMemory('bom', Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), jsonBytes({ memories: ['QA bom'] })]));
addMemory('invalid_utf8', Buffer.concat([Buffer.from('{"memories":["QA '), Buffer.from([0xc0, 0xaf, 0xe0, 0x80, 0xbf, 0xf4, 0x90, 0x80, 0x80]), Buffer.from('"]}')]));
addMemory('memories_not_array', jsonBytes({ memories: 'QA likes tea' }));
addMemory('memories_object', jsonBytes({ memories: { 0: 'QA zero' } }));
addMemory('memories_mixed', jsonBytes({ memories: ['QA likes tea', 5, null, '', '   ', ['QA nested'], { qa: 1 }, '  QA padded  ', ' 　QA wide spaces﻿', 'QA likes tea'] }));
addMemory('memories_duplicates_after_trim', jsonBytes({ memories: ['QA dup', ' QA dup ', 'QA dup\n', 'QA Dup'] }));
addMemory('memories_long', jsonBytes({ memories: ['m'.repeat(300), 'n'.repeat(301), `${'o'.repeat(299)}😀`, `${'p'.repeat(298)}😀tail`, `  ${'q'.repeat(305)}`] }));
addMemory('memories_51', jsonBytes({ memories: Array.from({ length: 53 }, (_, i) => (i === 3 ? '   ' : `QA fact ${i}`)) }));
addMemory('memories_unicode', jsonBytes({ memories: ['QA 日本語', 'QA 😀', '\ud83dQA lone', 'QA\nline\nbreaks', 'QA sep', '- QA dash'] }));
addMemory('use_project_false', jsonBytes({ memories: ['QA account'], useProjectMemories: false }), ['QA project']);
for (const useProjectMemories of [true, false, 0, null, 'false', undefined]) {
  addMemory(`use_project_${JSON.stringify(useProjectMemories)}`, jsonBytes({ memories: ['QA account', 'QA shared'], useProjectMemories }), ['QA shared', 'QA project', ' QA project ', '', '  ', 5, null, 'QA project']);
}
addMemory('project_not_array', jsonBytes({ memories: ['QA account'] }), 'QA project');
addMemory('project_only', jsonBytes({}), ['QA project only']);
addMemory('project_null', jsonBytes({ memories: ['QA account'] }), null);
for (let n = 0; n < 20; n += 1) {
  const list = Array.from({ length: int(0, 8) }, () => (rand() < 0.15 ? pick([5, null, '', ' ']) : words(int(1, 10))));
  addMemory(`random_${n}`, jsonBytes({ memories: list, ...(rand() < 0.3 ? { useProjectMemories: false } : {}) }), rand() < 0.4 ? [words(3), list[0] ?? 'QA'] : undefined);
}

// ── Fixed now (NOEVIA_CONTRACT_FIXED_NOW) ───────────────────────────────────────────────────
const fixedNow = [];
const SENTINEL = 1234567890123;
const RealDate = Date;
for (const raw of [undefined, '', '   ', '2026-10-12T12:00:00Z', ' 2026-10-12T12:00:00Z\n', '2026-10-12T12:00:00.123Z', '2026-10-12T12:00:00.12Z',
  '2026-10-12T12:00:00+00:00', '2026-10-12T12:00:00z', '2026-10-12 12:00:00Z', '2026-10-12', '2026-02-29T00:00:00Z', '2024-02-29T00:00:00Z',
  '2026-02-30T00:00:00Z', '2026-04-31T00:00:00Z', '2026-13-01T00:00:00Z', '2026-00-10T00:00:00Z', '2026-10-00T00:00:00Z',
  '2026-10-12T24:00:00Z', '2026-10-12T23:59:59Z', '2026-10-12T23:60:00Z', '2026-10-12T23:59:60Z', '0000-01-01T00:00:00Z', '9999-12-31T23:59:59.999Z',
  '1900-02-29T00:00:00Z', '2000-02-29T00:00:00Z', '1970-01-01T00:00:00Z', '1969-12-31T23:59:59.999Z', '١٢٣٤-01-01T00:00:00Z', 'yesterday',
  ' 2026-10-12T12:00:00Z﻿', '2026-10-12T12:00:00Z ', '+002026-10-12T12:00:00Z']) {
  const warnings = [];
  const saved = console.warn;
  console.warn = (...a) => warnings.push(a.map(String).join(' '));
  globalThis.Date = class extends RealDate { constructor(...a) { if (a.length) super(...a); else super(SENTINEL); } };
  let now;
  try { now = currentTime.currentNow(raw === undefined ? {} : { NOEVIA_CONTRACT_FIXED_NOW: raw }); } finally { globalThis.Date = RealDate; console.warn = saved; }
  const ms = now.getTime();
  fixedNow.push({ raw: raw === undefined ? null : raw, pinned: ms === SENTINEL ? null : ms, warnings });
}

// ── Time part ───────────────────────────────────────────────────────────────────────────────
const time = [];
function addTime(name, ms, zone, zoneName = true) {
  time.push({ name, ms, zone, zoneName, part: currentTime.systemPart(new RealDate(ms), zone, { zoneName }) });
}
const ZONES = ['UTC', 'Europe/Oslo', 'America/New_York', 'Asia/Kolkata', 'Australia/Lord_Howe', 'Pacific/Kiritimati', 'Pacific/Pago_Pago',
  'America/St_Johns', 'Asia/Kathmandu', 'Etc/GMT+12', 'Etc/GMT-14', 'Pacific/Chatham', 'America/Sao_Paulo', 'Asia/Tehran'];
const ODD_ZONES = ['europe/oslo', 'EUROPE/OSLO', 'US/Eastern', 'Asia/Calcutta', 'GMT', 'Etc/UTC', 'UCT', 'Zulu', 'EST', 'EST5EDT', 'Etc/GMT0',
  'Mars/QA_Base', '', ' UTC', 'utc', '+05:30', '-0800', 'Z', 'Local', 'America/Argentina/ComodRivadavia', 'Antarctica/Troll', 'America/Ciudad_Juarez',
  'Europe/Kyiv', 'Europe/Kiev', 'Pacific/Kanton', 'Asia/Rangoon', 'CET', 'PST8PDT'];
const utc = (y, mo, d, h = 12, mi = 0) => RealDate.UTC(y, mo - 1, d, h, mi);
const EDGES = [
  ['dec31_2020_week53', utc(2020, 12, 31)], ['jan1_2021', utc(2021, 1, 1)], ['jan3_2021', utc(2021, 1, 3)], ['jan4_2021', utc(2021, 1, 4)],
  ['dec28_2026', utc(2026, 12, 28)], ['dec31_2026', utc(2026, 12, 31)], ['jan1_2027', utc(2027, 1, 1)], ['oct12_2026', utc(2026, 10, 12)],
  ['nov13_2026', utc(2026, 11, 13)], ['feb29_2024', utc(2024, 2, 29)], ['dec31_2024', utc(2024, 12, 31)], ['feb28_2100', utc(2100, 2, 28)],
  ['mar1_2100', utc(2100, 3, 1)], ['feb29_2000', utc(2000, 2, 29)], ['dec31_2000', utc(2000, 12, 31)], ['jan1_1970', 0], ['epoch_minus1', -1],
  ['jan1_1900', utc(1900, 1, 1)], ['dec29_2025', utc(2025, 12, 29)], ['jan1_2010_week53', utc(2010, 1, 1)], ['jan2_2005', utc(2005, 1, 2)],
  ['dec31_2018_week1', utc(2018, 12, 31)], ['oslo_dst_start', utc(2026, 3, 29, 0, 59)], ['ny_dst_end', utc(2026, 11, 1, 5, 30)],
  ['midnight_utc', utc(2026, 11, 13, 0, 0)], ['before_midnight_utc', utc(2026, 11, 13, 23, 59)], ['year_9999', utc(9999, 12, 30, 12)],
  ['year_1583', utc(1583, 1, 1)], ['jan1_2033', utc(2033, 1, 1)], ['jan1_2038_after_i32', utc(2038, 1, 19, 3, 15)],
];
for (const [name, ms] of EDGES) for (const zone of ['UTC', 'Pacific/Kiritimati', 'Etc/GMT+12', 'Asia/Kathmandu']) addTime(`${name}@${zone}`, ms, zone);
for (const zone of ODD_ZONES) addTime(`odd_zone_${JSON.stringify(zone)}`, utc(2026, 12, 31, 23, 30), zone);
for (const zone of ZONES) addTime(`zone_${zone}_nameless`, utc(2026, 11, 13, 23, 30), zone, false);
addTime('unknown_zone_nameless', utc(2026, 11, 13), 'Mars/QA_Base', false);
for (let n = 0; n < 160; n += 1) {
  const ms = Math.floor(utc(1901, 1, 1) + rand() * (utc(2099, 12, 31) - utc(1901, 1, 1)));
  addTime(`random_${n}`, ms, pick(ZONES), rand() < 0.8);
}

// ── Local calendar dates near local midnight (the zone data, every day of 2025-2027) ─────────
// systemPart's day and calendarFacts come from the date `now` has in the zone; the date is what the zone data decides.
// Per zone: [ms, yyyymmdd] at 1 and 61 minutes either side of the local midnight that the zone's noon offset predicts,
// so offset changes at or near midnight (Ramadan, negative DST, abolished DST) are straddled.
const TZ_EDGE_ZONES = ['Africa/Casablanca', 'Europe/Dublin', 'America/Asuncion', 'Asia/Almaty', 'America/Nuuk'];
const zoneDays = [];
const ymd = (ms, zone) => {
  const p = {};
  for (const x of new Intl.DateTimeFormat('en-US', { timeZone: zone, year: 'numeric', month: 'numeric', day: 'numeric', calendar: 'iso8601', numberingSystem: 'latn' }).formatToParts(new RealDate(ms))) p[x.type] = x.value;
  return Number(p.year) * 10000 + Number(p.month) * 100 + Number(p.day);
};
const offsetMinutes = (ms, zone) => {
  const name = new Intl.DateTimeFormat('en-US', { timeZone: zone, timeZoneName: 'longOffset' }).formatToParts(new RealDate(ms)).find((x) => x.type === 'timeZoneName').value;
  const m = /^GMT(?:([+-])(\d{2}):(\d{2}))?$/.exec(name);
  return m && m[1] ? (m[1] === '-' ? -1 : 1) * (Number(m[2]) * 60 + Number(m[3])) : 0;
};
for (const zone of TZ_EDGE_ZONES) {
  const cases = [];
  for (let day = RealDate.UTC(2025, 0, 1); day <= RealDate.UTC(2027, 11, 31); day += 86400000) {
    const midnight = day - offsetMinutes(day + 43200000, zone) * 60000;
    for (const delta of [-61, -1, 1, 61]) { const ms = midnight + delta * 60000; cases.push([ms, ymd(ms, zone)]); }
  }
  zoneDays.push({ zone, cases });
}
for (const zone of TZ_EDGE_ZONES) {
  for (const [y, mo, d] of [[2025, 3, 1], [2025, 6, 15], [2026, 2, 20], [2026, 10, 25], [2027, 1, 1]]) {
    addTime(`edge_zone_${zone}_${y}-${mo}-${d}`, utc(y, mo, d, 12), zone);
    addTime(`edge_zone_${zone}_${y}-${mo}-${d}_nameless`, utc(y, mo, d, 12), zone, false);
  }
}

// ── normalizeReplayHistory on mapped histories ──────────────────────────────────────────────
const TOOL_NAMES = ['qa_web_lookup', 'qa_wiki_page', 'qa_private_read', 'read_project_file', 'qa.tool-name_1', 'qa tool!', 'q'.repeat(80),
  'q'.repeat(81), '😀qa', '', 'qa_unknown'];
const PUBLIC = new Set(['qa_web_lookup', 'qa_wiki_page']);
const KNOWN = new Set(['qa_web_lookup', 'qa_wiki_page', 'qa_private_read', 'read_project_file']);
const carryOrphanTool = (name) => typeof name === 'string' && PUBLIC.has(name);
const knownTool = (name) => typeof name === 'string' && KNOWN.has(name);
const contentOf = () => {
  const r = rand();
  if (r < 0.1) return 'x'.repeat(int(495, 505)) + pick(['', '😀', '\ud83d', 'tail']);
  if (r < 0.15) return `${'y'.repeat(499)}😀${'z'.repeat(3)}`;
  if (r < 0.2) return `QA ${words(2)} </untrusted> ${words(2)}`;
  return words(int(1, 12));
};
function mappedEntry() {
  const r = rand();
  if (r < 0.3) return { role: 'user', content: contentOf() };
  if (r < 0.55) return { role: 'assistant', content: contentOf() };
  const role = rand() < 0.8 ? 'tool' : 'function';
  const name = rand() < 0.85 ? pick(TOOL_NAMES) : undefined;
  if (role === 'tool' && name !== undefined && rand() < 0.25) {
    const e = { role: 'tool', content: rand() < 0.2 ? '' : contentOf(), name, applied: true };
    if (rand() < 0.6) e.target = pick(['', 'QA/notes.md', 't'.repeat(int(299, 302)), `${'u'.repeat(299)}😀`]);
    if (rand() < 0.6) e.args = pick(['', '{"qa":1}', 'a'.repeat(int(299, 302)), words(5)]);
    return e;
  }
  if (role === 'tool' && name !== undefined && rand() < 0.25) return { role: 'tool', content: contentOf(), name, declined: true };
  return name === undefined ? { role, content: contentOf() } : { role, content: contentOf(), name };
}
const replay = [];
function addReplay(name, mapped, newMessage, offBox) {
  const input = JSON.parse(JSON.stringify(mapped)); // normalizeReplayHistory does not change its input; recorded as given
  const out = normalizeReplayHistory(mapped, newMessage, { carryOrphanTool, offBox, knownTool });
  replay.push({ name, mapped: input, ...(newMessage === undefined ? {} : { newMessage }), offBox, out });
}
const U = (content) => ({ role: 'user', content }), A = (content) => ({ role: 'assistant', content });
const T = (name, content, extra = {}) => ({ role: 'tool', content, ...(name === undefined ? {} : { name }), ...extra });
const HAND = {
  empty: [], one_user: [U('QA hi')], exchange: [U('QA hi'), A('QA hello')], two_users: [U('QA a'), U('QA b')], three_users: [U('a'), U('b'), U('c')],
  starts_assistant: [A('QA orphan'), A('QA second'), U('QA q'), A('QA a')], only_assistants: [A('a'), A('b')],
  merge_assistants: [U('q'), A('one'), A('two'), A('three')],
  tool_after_assistant: [U('q'), A('QA a'), T('qa_web_lookup', 'QA result')], tool_first: [T('qa_web_lookup', 'QA lost'), U('q')],
  tool_after_user_public: [U('q'), T('qa_web_lookup', 'QA carried'), T('qa_private_read', 'QA private'), T('qa_wiki_page', 'QA wiki'), A('QA answer')],
  tool_after_user_private: [U('q'), T('qa_private_read', 'QA private'), T('qa_web_lookup', 'QA after')],
  function_role: [U('q'), A('a'), { role: 'function', content: 'QA fn', name: 'qa_web_lookup' }, { role: 'function', content: 'QA fn2' }],
  applied_first: [T('qa_private_read', 'QA done', { applied: true, target: 'QA/x.md', args: '{"qa":1}' }), U('QA first'), A('ok')],
  applied_first_no_user: [T('qa_private_read', 'QA done', { applied: true })],
  applied_after_user: [U('q'), T('qa.tool-name_1', '', { applied: true, target: '', args: '' })],
  applied_after_assistant: [U('q'), A('a'), T('qa tool!', 'QA r', { applied: true, target: 'QA/t', args: 'x' })],
  declined_first: [T('qa_private_read', 'QA no', { declined: true }), U('q')], declined_after_user: [U('q'), T('qa_web_lookup', 'QA no', { declined: true })],
  declined_after_assistant: [U('q'), A('a'), T('read_project_file', 'QA no', { declined: true })],
  carrier_then_assistant: [U('q'), T('qa_web_lookup', 'QA r1'), A('QA merged'), T('qa_private_read', 'QA p'), T('qa_wiki_page', 'QA w')],
  carrier_then_applied: [U('q'), T('qa_web_lookup', 'QA r1'), T('qa_private_read', 'QA d', { applied: true }), T('qa_private_read', 'QA p2')],
  ends_user_then_new: [U('q'), A('a'), U('QA abandoned')], leading_two_applied: [T('a', 'r1', { applied: true }), T('b', 'r2', { applied: true }), A('x'), U('q')],
  name_lengths: [U('q'), A('a'), T('q'.repeat(80), 'QA 80', { applied: true }), T('q'.repeat(81), 'QA 81', { applied: true }), T('😀qa', 'QA astral', { declined: true }), T('', 'QA empty', { declined: true })],
  label_cut_astral: [U('q'), A('a'), T(`${'l'.repeat(79)}😀`, 'QA label'), T(`${'l'.repeat(78)}😀`, 'QA label2')],
  dollar_patterns: [U('$& $1 $$'), A("$' $`"), T('qa_web_lookup', '$& result')],
};
for (const [name, mapped] of Object.entries(HAND)) {
  for (const offBox of [false, true]) {
    addReplay(`${name}_new_${offBox ? 'off' : 'on'}`, mapped, 'QA new message', offBox);
    addReplay(`${name}_compact_${offBox ? 'off' : 'on'}`, mapped, undefined, offBox);
  }
}
addReplay('new_message_empty', [U('q'), A('a')], '', false);
addReplay('new_message_unicode', [U('q'), A('a')], '\ud83dQA 😀  ', false);
for (let n = 0; n < 220; n += 1) {
  const mapped = Array.from({ length: int(0, 14) }, mappedEntry);
  addReplay(`random_${n}`, mapped, rand() < 0.85 ? words(int(1, 6)) : undefined, rand() < 0.4);
}

// ── Differences (Node accepts, the port does not state it as Node does) ────────────────────
const differences = [];
const escaped = (v) => JSON.stringify(v).replace(/[^\x00-\x7f]/g, (c) => `\\u${c.charCodeAt(0).toString(16).padStart(4, '0')}`);
function differenceFile(kind, name, about, bytes) {
  const lib = kind === 'account' ? instructions : accountMemory;
  const record = lib.read(dirWith({ [kind === 'account' ? 'account-instructions.json' : 'account-memory.json']: bytes }));
  differences.push({ kind, name, about, fileHex: hex(bytes), node: escaped({ record, part: lib.systemPart(record) }) });
}
differenceFile('account', 'account_number_out_of_range', 'A number outside the f64 range (JSON.parse reads Infinity): wtf16::parse refuses the file, so the port reads the empty record.',
  Buffer.from('{"text":"QA kept","updatedAt":1e400}'));
differenceFile('account', 'account_plane_16_tail', 'A character of U+10F800..U+10FFFF (wtf16 holds lone surrogates there): wtf16::parse refuses the file.',
  Buffer.from('{"text":"QA \\udbfe\\udc00 tail"}'));
differenceFile('memory', 'memory_number_out_of_range', 'A number outside the f64 range: the port reads the empty record.',
  Buffer.from('{"memories":["QA kept"],"updatedAt":-1e400}'));
differenceFile('memory', 'memory_deep_nesting', 'Nesting deeper than serde_json follows (128): the port reads the empty record.',
  Buffer.from(`{"memories":["QA kept"],"qa":${'['.repeat(200)}${']'.repeat(200)}}`));
for (const [name, ms] of [['time_julian_1582', RealDate.UTC(1582, 9, 4, 12)], ['time_year_0', -62167219200000], ['time_year_1', -62135596800000]]) {
  differences.push({ kind: 'time', name, about: 'Before 1583 Node formats the day in the Julian calendar; the port states only Gregorian years 1583-9999.', ms, zone: 'UTC',
    node: escaped(currentTime.systemPart(new RealDate(ms), 'UTC')) });
}

for (const zone of ['PST', 'AET', 'SystemV/AST4']) {
  const ms = utc(2026, 12, 31, 23, 30);
  differences.push({ kind: 'zone_id', name: `zone_id_${zone}`, about: 'A legacy ICU zone ID that is not in the IANA database: Node states the day in it, the port falls back to UTC.', ms, zone,
    node: escaped(currentTime.systemPart(new RealDate(ms), zone)) });
}
{
  const ms = utc(2026, 11, 13, 23, 30);
  differences.push({ kind: 'zone_data', name: 'zone_data_casablanca', about: "Node's ICU time-zone data and chrono-tz's IANA data disagree on Morocco's offset here.", ms, zone: 'Africa/Casablanca',
    node: escaped(currentTime.systemPart(new RealDate(ms), 'Africa/Casablanca')) });
}

// ── Whole turns through handleChat ──────────────────────────────────────────────────────────
const BOXES = [
  { id: 'web-search', tools: [{ function: { name: 'qa_web_lookup' } }] },
  { id: 'offline-wikipedia', tools: [{ function: { name: 'qa_wiki_page' } }] },
  { id: 'qa-private-box', tools: [{ function: { name: 'qa_private_read' } }] },
];
const turnTools = {
  public: TOOL_NAMES.filter((n) => toolsMode.isPublicToolName(n, BOXES)),
  known: TOOL_NAMES.filter((n) => toolsMode.PRIVATE_CORE_TOOLS.has(n) || BOXES.some((b) => b.tools.some((t) => t.function.name === n))),
};
function handlerFor(dir, cap, requests) {
  const fetch = async (url, init) => {
    requests.push(JSON.parse(init.body));
    return { ok: true, body: (async function* () { yield Buffer.from(`data: ${JSON.stringify({ choices: [{ delta: { content: 'QA ok' } }] })}\n\n`); })() };
  };
  return createChatHandler({
    modelManager: { enabled: true, health: async () => ({ ok: true, body: { all_models_loaded: [{ model_name: 'qa-model', loaded: true, recipe_options: { ctx_size: 1048576 } }] } }) },
    reasoningEffort: req('reasoning-effort.cjs'), authService: { audit() {} }, crypto, path, fs, fetch,
    HISTORY_CAP: cap, DEFAULT_PROVIDER_ID: 'default', createToolExchange: () => async () => 'unused',
    currentWorkspace: () => ({ userId: 'qa-user', dir, assetDir: () => '/qa-only' }),
    getProject: () => null, skillsIndexFor: () => [], getProvider: () => ({ id: 'default', baseUrl: 'http://qa-default.invalid' }), providerHeaders: () => ({}),
    autoRoles: () => null, visionDescriptions: new Map(), visionProbe: async () => ({ supported: false, reason: 'qa' }),
    chatSkillRouter: { select: async () => ({ loaded: [] }) }, oauthServerIds: () => new Set(), accountReady: () => true, mcpOAuth: { connected: () => false },
    chatToolRouter: { select: async (ids) => ({ ids, routed: false }) }, DEFAULT_TOOLBOXES: [], CONNECTOR_BOXES: new Set(), connectedBoxes: () => [],
    toolPolicy: { mode: () => 'allow' }, requestScope: { getStore: () => ({}) }, resolveTools: () => ({ tools: [], dropped: [] }), isWriteTool: () => false,
    rag: { filesContext: async () => null }, prefill: { recordSample() {} }, reduceToolResult: () => ({ text: 'reduced' }), diaryExtras: req('diary-extras.cjs'),
    DIARY_BASE: 'http://qa.invalid', TOOL_RESULT_CAP: 8000, json: (res, status, body) => { throw new Error(`QA turn answered ${status}: ${JSON.stringify(body)}`); },
    saveChats() {}, endpointApproved: () => true, diaryHeaders: () => ({}), lastLoadedModel: () => 'qa-model', classifyFastOrSmart: async () => 'fast',
    servedCatalogue: async () => [], modelsInstalled: async () => [], missingRoles: () => [], staleRolesError: () => null, allToolboxes: () => BOXES,
    chatWideApproved: () => false, recordUsage() {}, recordToolUse() {}, executeToolCall: async () => 'unused', awaitApproval: async () => 'deny',
  });
}
const turns = [];
async function addTurn(name, { instructionsBytes = null, memoryBytes = null, pin = '2026-11-13T12:00:00Z', tz = 'UTC', cap = 1000, message = 'QA question', history }) {
  const dir = dirWith({ 'account-instructions.json': instructionsBytes, 'account-memory.json': memoryBytes });
  process.env.NOEVIA_CONTRACT_FIXED_NOW = pin;
  process.env.TZ = tz;
  const zone = currentTime.serverTimeZone();
  const requests = [], events = [];
  const res = new EventEmitter();
  res.writeHead = () => {}; res.setHeader = () => {};
  res.write = (line) => { events.push(String(line)); return true; };
  res.end = () => { res.writableEnded = true; res.emit('finish'); };
  const { handleChat } = handlerFor(dir, cap, requests);
  await handleChat({ headers: {} }, res, { message, ...(history === undefined ? {} : { history }) });
  if (requests.length !== 1) throw new Error(`${name}: ${requests.length} model requests; events: ${events.join('').slice(0, 400)}`);
  turns.push({ name, instructionsHex: instructionsBytes && hex(instructionsBytes), memoryHex: memoryBytes && hex(memoryBytes), pin, zone, cap, message,
    ...(history === undefined ? {} : { history }), messages: requests[0].messages });
}
const SAFE_WORDS = ['QA', 'alpha', 'notes', 'Ünïcode', '日本語', '😀', '𝒳qa', 'tab\there', 'line\nbreak', 'quote"qa', ' sep', '\ud83d', '\ude00', '</untrusted>', '$&'];
const safeWords = (n) => Array.from({ length: n }, () => pick(SAFE_WORDS)).join(' ');
function rawEntry() {
  const r = rand();
  if (r < 0.04) return pick([null, 'QA string entry', 7, ['user', 'QA'], true, 0, '']);
  const role = rand() < 0.92 ? pick(['user', 'assistant', 'tool', 'function']) : pick(['system', 'User', 'developer', undefined, 5]);
  const e = { role };
  const c = rand();
  e.content = c < 0.75 ? (rand() < 0.08 ? 'w'.repeat(int(495, 505)) : safeWords(int(1, 8))) : c < 0.83 ? '' : pick([undefined, null, 5, ['QA'], { text: 'QA' }, true]);
  if ((role === 'tool' || role === 'function') && rand() < 0.85) e.name = rand() < 0.9 ? pick(TOOL_NAMES) : pick([5, null, ['qa_web_lookup']]);
  if (role === 'tool' || rand() < 0.05) {
    if (rand() < 0.3) e.applied = rand() < 0.85 ? true : pick(['true', 1, false]);
    if (rand() < 0.25) e.declined = rand() < 0.85 ? true : pick(['true', 1]);
    if (rand() < 0.4) e.target = rand() < 0.85 ? pick(['QA/notes.md', '', 'v'.repeat(301)]) : 9;
    if (rand() < 0.4) e.args = rand() < 0.85 ? pick(['{"qa":true}', '', 'a'.repeat(303)]) : { qa: 1 };
  }
  if (role === 'assistant' && rand() < 0.15) e.failure = pick(['QA stopped', '   ', '', 5, null, ' QA　']);
  if (rand() < 0.05) e.extra = 'QA extra field';
  return e;
}
const fullInstructions = jsonBytes({ text: 'QA standing instruction', style: 'concise', advanced: { length: 'short', tone: 'auto', formatting: 'minimal', emoji: 'none' }, language: 'Norwegian' });
const fullMemory = jsonBytes({ memories: ['QA prefers tea', '  QA lives in Oslo  ', 'QA prefers tea'] });

(async () => {
  await addTurn('bare', {});
  await addTurn('no_history_field', { history: undefined });
  await addTurn('history_not_array', { history: { 0: { role: 'user', content: 'QA' } } });
  await addTurn('history_string', { history: 'QA' });
  await addTurn('account_and_memory', { instructionsBytes: fullInstructions, memoryBytes: fullMemory, history: [U('QA earlier'), A('QA reply')] });
  await addTurn('memory_only', { memoryBytes: fullMemory });
  await addTurn('account_only', { instructionsBytes: fullInstructions });
  await addTurn('broken_files', { instructionsBytes: Buffer.from('{'), memoryBytes: Buffer.from('nope') });
  await addTurn('time_zone_oslo', { tz: 'Europe/Oslo', pin: '2026-12-31T23:30:00Z' });
  await addTurn('time_zone_kiritimati', { tz: 'Pacific/Kiritimati', pin: '2026-12-31T10:30:00Z' });
  await addTurn('time_zone_new_york', { tz: 'America/New_York', pin: '2027-01-01T03:00:00Z' });
  await addTurn('message_unicode', { message: '\ud83dQA 😀 日本語   $& </untrusted>' });
  await addTurn('message_empty_string_with_history', { message: ' ', history: [U('q')] });
  await addTurn('failed_turns', { history: [U('q1'), { role: 'assistant', content: 'QA partial', failure: 'QA stopped' }, U('q2'), { role: 'assistant', content: 'QA kept', failure: '  ' }] });
  await addTurn('applied_without_content', { history: [U('q'), A('a'), { role: 'tool', name: 'qa_private_read', content: '', applied: true, target: 'QA/x', args: '{}' }] });
  await addTurn('applied_and_declined', { history: [U('q'), { role: 'tool', name: 'qa_web_lookup', content: 'QA both', applied: true, declined: true }, { role: 'tool', name: 'qa_private_read', content: 'QA no', declined: true }] });
  await addTurn('function_applied', { history: [U('q'), A('a'), { role: 'function', name: 'qa_web_lookup', content: 'QA fn', applied: true }] });
  await addTurn('cap_3', { cap: 3, history: [U('q1'), A('a1'), U('q2'), A('a2'), U('q3'), A('a3')] });
  await addTurn('cap_1', { cap: 1, history: [U('q1'), A('a1')] });
  await addTurn('cap_0_keeps_all', { cap: 0, history: [U('q1'), A('a1'), U('q2'), A('a2')] });
  await addTurn('cap_counts_after_filter', { cap: 2, history: [U('q1'), A('a1'), null, { role: 'system', content: 'x' }, A(''), { role: 'assistant', content: 'f', failure: 'x' }, U('q2')] });
  await addTurn('cap_1000_over', { history: Array.from({ length: 1003 }, (_, i) => (i % 2 ? A(`a${i}`) : U(`q${i}`))) });
  await addTurn('cap_1000_exact', { history: Array.from({ length: 1000 }, (_, i) => (i % 2 ? U(`q${i}`) : A(`a${i}`))) });
  for (let n = 0; n < 90; n += 1) {
    await addTurn(`random_${n}`, {
      instructionsBytes: rand() < 0.3 ? fullInstructions : null, memoryBytes: rand() < 0.3 ? fullMemory : null,
      tz: pick(['UTC', 'Europe/Oslo', 'Asia/Kathmandu']), cap: pick([1000, 1000, 1000, 4, 2]), message: safeWords(int(1, 5)),
      history: Array.from({ length: int(0, 16) }, rawEntry),
    });
  }
  delete process.env.NOEVIA_CONTRACT_FIXED_NOW;

  // ── Output ──────────────────────────────────────────────────────────────────────────────
  const lines = (list) => list.map((x) => JSON.stringify(x)).join(',\n');
  const text = `{"version":1,"generator":"crates/chat-prepare/tests/record_prepare.cjs",\n"node":${JSON.stringify(process.version)},"icu":${JSON.stringify(process.versions.icu)},"tz":${JSON.stringify(process.versions.tz)},\n`
    + `"sources":${JSON.stringify(sources)},\n"replayTools":${JSON.stringify({ public: [...PUBLIC], known: [...KNOWN] })},\n`
    + `"turnTools":${JSON.stringify(turnTools)},\n`
    + `"account":[\n${lines(account)}\n],\n"accountRaw":[\n${lines(accountRaw)}\n],\n"memory":[\n${lines(memory)}\n],\n`
    + `"fixedNow":[\n${lines(fixedNow)}\n],\n"time":[\n${lines(time)}\n],\n"zoneDays":[\n${lines(zoneDays)}\n],\n"replay":[\n${lines(replay)}\n],\n"turns":[\n${lines(turns)}\n],\n"differences":[\n${lines(differences)}\n]}\n`;
  const check = process.argv.indexOf('--check');
  const counts = `${account.length} account, ${accountRaw.length} accountRaw, ${memory.length} memory, ${fixedNow.length} fixedNow, ${time.length} time, ${zoneDays.reduce((n, z) => n + z.cases.length, 0)} zoneDays, ${replay.length} replay, ${turns.length} turns, ${differences.length} differences`;
  if (check > 0) {
    const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
    if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
    process.stderr.write(`ok: ${counts} recorded as committed\n`);
  } else {
    process.stderr.write(`recorded: ${counts}\n`);
    process.stdout.write(text);
  }
})().catch((e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
