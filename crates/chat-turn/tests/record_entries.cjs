#!/usr/bin/env node
'use strict';
// Recorded turn records for crates/chat-turn's `record` module: the contract it is replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-turn/tests/record_entries.cjs > crates/chat-turn/tests/fixtures/node-turn-record.v1.json
//   ... node crates/chat-turn/tests/record_entries.cjs --check crates/chat-turn/tests/fixtures/node-turn-record.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every case drives noevia-core's own functions over synthetic QA data: chat-interrupted-turn.cjs createTurnRecord
// (injected clock) fed a case's events, then chat-partial.cjs on the entry chat.cjs keeps (`partialEntry() ||
// assistantEntry()`): bounded() (not exported, so the module source is compiled once more with an export line) and
// the file startPartialSaves().finish({ keep }) writes (temporary folder, read back). `random_*` cases are seeded
// sequences from the vocabulary; `boundedCases` give bounded() entries directly.
// A case records `clock` (what now() returns in order, the last repeating: the start, then one per usage), `now`
// (Date.now() for savedAt), `events` (`threw`: the ones observe() threw on), and Node's assistantEntry,
// partialEntry, bounded and partialFile, each as Node's JSON.stringify text ("null" where Node gives none). Events and
// bounded() entries are JSON text too (some written by hand: `2.0`, `-0`, `1e2`), so Rust parses what Node parsed;
// lone surrogates in them are Node's `\udxxx` escapes.
// Strings over 256 units with a repeat are written { "$str": [segment, ...] }: literal text, or [text, count] for a repeat.
// The two source files' sha256 are recorded: a changed Node file needs a fresh recording.

process.env.TZ = 'UTC';
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const Module = require('node:module');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const core = path.resolve(checkout);
const SOURCES = { 'server/chat-interrupted-turn.cjs': null, 'server/chat-partial.cjs': null };
for (const rel of Object.keys(SOURCES)) {
  SOURCES[rel] = crypto.createHash('sha256').update(fs.readFileSync(path.join(core, rel))).digest('hex');
}
for (const level of ['log', 'info', 'warn', 'error']) console[level] = (...args) => process.stderr.write(`${args.map(String).join(' ')}\n`);

// bounded() and finish() stamp savedAt with Date.now(); it reads this value.
const FIXED_NOW = Date.UTC(2026, 10, 13, 12, 0, 0);
let wallNow = FIXED_NOW;
Date.now = () => wallNow;

const { createTurnRecord } = require(path.join(core, 'server', 'chat-interrupted-turn.cjs'));
const partials = require(path.join(core, 'server', 'chat-partial.cjs'));
// The same source once more, with bounded() exported: the function itself, not a copy of it.
function withBounded(file) {
  const m = new Module(file, module);
  m.filename = file;
  m.paths = Module._nodeModulePaths(path.dirname(file));
  m._compile(`${fs.readFileSync(file, 'utf8')}\nmodule.exports.bounded = bounded;\n`, file);
  return m.exports.bounded;
}
const bounded = withBounded(path.join(core, 'server', 'chat-partial.cjs'));
if (partials.PARTIAL_CAP_CHARS !== 65536) throw new Error(`PARTIAL_CAP_CHARS is ${partials.PARTIAL_CAP_CHARS}`);

const workDir = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-turn-record-'));
// What finish({ keep }) leaves in the chat's partial file.
function finishedFile(keep) {
  const chatId = 'qa-chat-record';
  const saves = partials.startPartialSaves({ dir: workDir, chatId, record: { partialEntry: () => null }, everyMs: 3600000 });
  saves.finish({ keep });
  const file = partials.fileFor(workDir, chatId);
  const written = fs.readFileSync(file, 'utf8'); // JSON.stringify's text: lone surrogates escaped, ASCII-safe
  fs.rmSync(file);
  return written;
}

// ── Long strings, written compactly ─────────────────────────────────────────────────────────
function encode(value) {
  if (typeof value === 'string' && value.length > 256) {
    const segments = [];
    const chars = Array.from(value);
    for (let i = 0; i < chars.length;) {
      let j = i;
      while (j < chars.length && chars[j] === chars[i]) j++;
      if (j - i >= 8) segments.push([chars[i], j - i]);
      else if (typeof segments[segments.length - 1] === 'string') segments[segments.length - 1] += chars.slice(i, j).join('');
      else segments.push(chars.slice(i, j).join(''));
      i = j;
    }
    return segments.some(Array.isArray) && segments.length < 64 ? { $str: segments } : value;
  }
  if (Array.isArray(value)) return value.map(encode);
  if (value && typeof value === 'object') {
    if ('$str' in value) throw new Error('a recorded object has a $str key');
    return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, encode(v)]));
  }
  return value;
}

// ── Running one case ────────────────────────────────────────────────────────────────────────
// An event or entry as JSON text: a Raw is written by hand (number forms JSON.stringify never makes).
class Raw { constructor(text) { this.text = text; } }
const raw = (text) => new Raw(text);
const asText = (v) => (v instanceof Raw ? v.text : JSON.stringify(v));
const text = (v) => (v === undefined ? 'null' : JSON.stringify(v));

function run(c) {
  const events = c.events.map(asText);
  const clock = c.clock || [FIXED_NOW];
  let tick = 0;
  const record = createTurnRecord({ now: () => clock[Math.min(tick++, clock.length - 1)] });
  const threw = [];
  events.forEach((ev, i) => { try { record.observe(JSON.parse(ev)); } catch { threw.push(i); } });
  const now = c.now ?? FIXED_NOW;
  wallNow = now;
  const assistantEntry = record.assistantEntry();
  const partialEntry = record.partialEntry();
  const keep = partialEntry || assistantEntry;
  const out = { assistantEntry: text(assistantEntry), partialEntry: text(partialEntry),
    bounded: text(keep ? bounded(keep) : null), partialFile: keep ? finishedFile(keep) : 'null' };
  wallNow = FIXED_NOW;
  return encode({ name: c.name, about: c.about, clock, now, events, ...(threw.length ? { threw } : {}), ...out });
}

function runBounded(c) {
  const entry = asText(c.entry);
  wallNow = c.now;
  const out = { bounded: text(bounded(JSON.parse(entry))), partialFile: finishedFile(JSON.parse(entry)) };
  wallNow = FIXED_NOW;
  return encode({ name: c.name, about: c.about, now: c.now, entry, ...out });
}

// ── Vocabulary ──────────────────────────────────────────────────────────────────────────────
const E = '\u{1F600}'; // an astral character: two UTF-16 units
const a = (n) => 'a'.repeat(n);
const meta = (o = {}) => ({ type: 'meta', ...o });
const delta = (text) => ({ type: 'delta', text });
const reasoning = (text) => ({ type: 'reasoning', text });
const preamble = (text) => ({ type: 'preamble', text });
const tool = (o = {}) => ({ type: 'tool', name: 'qa_read', args: '{"path":"qa.txt"}', ...o });
const pending = (o = {}) => ({ type: 'tool_pending', name: 'qa_write', args: '{"path":"qa.txt"}', ...o });
const result = (o = {}) => ({ type: 'tool_result', name: 'qa_read', text: 'QA result', ...o });
const paused = (o = {}) => ({ type: 'paused', ...o });
const usage = (o = {}) => ({ type: 'usage', promptTokens: 12, completionTokens: 34, totalTokens: 46, tokensPerSecond: 7.5, ...o });
const error = { type: 'error', message: 'QA failure' };
const routed = meta({ route: 'fast', routingDecision: { offered: ['fast'], effectiveRole: 'fast' }, routing: { route: 'local', reason: 'auto' } });
const T0 = FIXED_NOW;

const CASES = [
  { name: 'no_events', about: 'Nothing streamed: the Stopped placeholder.', events: [] },
  { name: 'plain_answer', about: 'A routed answer with sources and usage.',
    clock: [T0, T0 + 1500], events: [routed, { type: 'sources', sources: [{ title: 'QA source', url: 'https://qa.invalid/a' }] }, reasoning('QA thinking.'), delta('QA answer.'), usage()] },
  { name: 'unknown_and_non_object_events', about: 'Unknown types, a non-string type and non-objects are ignored.',
    events: [null, 'delta', 5, [], {}, { type: 42, text: 'x' }, { type: 'DELTA', text: 'x' }, { type: 'done' }, { type: 'telemetry', phase: 'streaming' }, { type: 'title', title: 'QA' }, delta('QA kept.')] },
  { name: 'non_string_texts', about: 'str(): every non-string text is empty.',
    events: [delta(5), delta(null), delta({ qa: 1 }), reasoning(true), preamble(7), delta('QA text.')] },
  { name: 'leading_space_trim_start', about: 'content.trimStart() with JS whitespace (U+FEFF, U+3000, U+2028); trailing space kept.',
    events: [delta('﻿　  \tQA answer with space after.  ')] },
  { name: 'nel_is_not_js_space', about: 'U+0085 is not JS whitespace: the content is kept.', events: [delta('\u0085')] },
  { name: 'zero_width_space_kept', about: 'U+200B and U+180E are not whitespace either.', events: [delta(' ​'), delta('᠎ ')] },
  { name: 'whitespace_only_content', about: 'Content that trims to nothing: the placeholder.', events: [delta(' \n﻿ 　\t')] },
  { name: 'preamble_removes_last_occurrence', about: 'lastIndexOf: the later copy of the narration leaves the content.',
    events: [delta('QA check. Then QA check. Done.'), preamble('QA check.')] },
  { name: 'preamble_without_match', about: 'Narration not in the content: content stays, thinking gets it trimmed.',
    events: [reasoning('QA thought.'), delta('QA answer.'), preamble('  QA narration  \n')] },
  { name: 'preamble_astral', about: 'An astral narration inside astral content.',
    events: [delta(`${E}QA${E} ${E}QA${E}`), preamble(`${E}QA${E}`)] },
  { name: 'preamble_whitespace_only', about: 'A blank narration: trimmed to nothing, the separator still goes in when the field has text.',
    events: [preamble('QA first.'), preamble('   '), preamble('QA third.'), tool({ index: 0 })] },
  { name: 'preamble_blank_first', about: 'A blank narration first adds nothing, not even a separator later.',
    events: [preamble(' \n '), preamble('QA second.')] },
  { name: 'narration_kept_after_tools', about: 'Cut off after tool steps before any answer: the narration is the text.',
    events: [routed, delta('QA let me look.'), preamble('QA let me look.'), tool({ index: 0 }), result({ index: 0 })] },
  { name: 'stopped_keeps_reasoning_in_partial', about: 'Only thinking: the assistant entry drops it, the partial entry keeps it raw.',
    events: [routed, reasoning('  QA unfinished thinking ')] },
  { name: 'stopped_blank_reasoning', about: 'Only blank thinking: no partial entry.', events: [reasoning(' \n ')] },
  { name: 'stopped_with_running_tool', about: 'A running call is stopped; the placeholder keeps the calls.',
    events: [tool({ index: 0 }), reasoning('QA thinking')] },
  { name: 'stopped_pending_approval', about: 'A pending approval card is stopped, target and kind kept.',
    events: [pending({ index: 0, target: 'QA file qa-file-1', targetKind: 'drive-new' })] },
  { name: 'failed_without_writes', about: 'A failed reply with no applied write is not kept.', events: [delta('QA partial'), tool({ index: 0 }), error] },
  { name: 'failed_with_writes', about: 'A failed reply keeps the record of its writes; declined calls left out.',
    events: [delta('QA text'), pending({ index: 0, target: 'QA file qa-file-2', targetKind: 'drive' }), result({ index: 0, name: 'qa_write', applied: true }),
      tool({ index: 1, name: 'qa_trash' }), result({ index: 1, name: 'qa_trash', declined: true, applied: true }), tool({ index: 2 }), error] },
  { name: 'failed_with_declined_pause', about: 'A failed reply after a declined pause keeps that pause.',
    events: [result({ index: 0, applied: true }), paused({ reason: 'declined', applied: 1, declined: ['qa_trash'] }), error] },
  { name: 'failed_only_denied_applied', about: 'The only applied call was declined: kept, with an empty call list.',
    events: [result({ index: 0, declined: true, applied: true }), error] },
  { name: 'applied_writes_note', about: 'Applied writes with no pause: the stopped note counts them.',
    events: [result({ index: 0, applied: true }), result({ index: 1, applied: true, notRun: true }), result({ index: 2, applied: 'true' }), delta('QA answer')] },
  { name: 'applied_only_no_text', about: 'Applied writes and no text: the note keeps the entry.', events: [result({ index: 0, applied: true })] },
  { name: 'pause_declined_list', about: 'The declined names: strings matching the tool name shape, deduplicated, at most 8.',
    events: [paused({ reason: 'declined', applied: 2, declined: ['qa_a', 'qa_b', 'qa_a', 'qa bad', '', 7, null, a(80), a(81), 'qa.c-d', 'é', 'qa\n', 'qa_1', 'qa_2', 'qa_3', 'qa_4', 'qa_5'] })] },
  { name: 'pause_declined_not_array', about: 'A declined list that is not an array is empty.', events: [paused({ reason: 'declined', declined: 'qa_a' })] },
  { name: 'pause_sensitive', about: 'sensitive-tool-result becomes sensitive.', events: [delta('QA'), paused({ reason: 'sensitive-tool-result', applied: 3 })] },
  { name: 'pause_supervision_default', about: 'Any other reason is supervision; the last pause wins.',
    events: [paused({ reason: 'declined', declined: ['qa_a'] }), paused({ reason: 'qa-other', applied: 1 })] },
  { name: 'pause_no_reason', about: 'A pause with no reason.', events: [paused()] },
  { name: 'pause_applied_kinds', about: 'applied: only a non-negative integer is kept.',
    events: [paused({ reason: 'sensitive-tool-result', applied: -1 }), delta('QA'), paused({ reason: 'supervision', applied: 1.5 }), paused({ reason: 'supervision', applied: '2' }), paused({ applied: 1e300 })] },
  { name: 'numbers_as_written', about: 'Number forms JSON.stringify never makes, parsed from the same text: 2.0, -0, 1e2, 1.50.',
    clock: [T0, T0 + 3], events: [raw('{"type":"paused","reason":"sensitive-tool-result","applied":2.0}'), raw('{"type":"delta","text":"QA"}'),
      raw('{"type":"tool","index":2.0,"name":"qa_two"}'), raw('{"type":"tool","index":-0,"name":"qa_zero"}'), raw('{"type":"tool","index":1e0,"name":"qa_one"}'),
      raw('{"type":"usage","promptTokens":-0,"completionTokens":1e2,"totalTokens":1.50,"tokensPerSecond":-1.25e-3}'),
      raw('{"type":"meta","route":1e2,"routingDecision":-0.0,"routing":{"route":"local","reason":"auto"}}')] },
  { name: 'numbers_negative_zero_applied', about: 'applied -0 is an integer >= 0: kept, written 0.',
    events: [raw('{"type":"paused","reason":"declined","applied":-0,"declined":["qa_a"]}')] },
  { name: 'numbers_route_float', about: 'A route 1.0 is labelled 1; routingDecision 0.0 is falsy.', events: [raw('{"type":"meta","route":1.0,"routingDecision":0.0}'), raw('{"type":"delta","text":"QA"}')] },
  { name: 'lone_halves_rejoin', about: 'An emoji split across two deltas is one character again.',
    events: [delta('QA \ud83d'), delta('\ude00 done'), reasoning('\ud83d'), reasoning('\ude00')] },
  { name: 'lone_half_stays', about: 'A lone high half with nothing after it, and a lone low one at the start.',
    events: [delta('\ude00QA \ud83d'), result({ index: 0, name: 'qa_\ud83d', args: 'x', text: '\udc00' })] },
  { name: 'preamble_removes_half_a_pair', about: 'lastIndexOf of a lone half finds it inside a pair; the other half is left lone.',
    events: [delta(`QA ${E} and ${E}.`), preamble('\ud83d')] },
  { name: 'preamble_low_half', about: 'Removing a lone low half; the narration is that half.',
    events: [delta(`${E}${E}`), preamble('\ude00'), tool({ index: 0 })] },
  { name: 'preamble_lone_after_trim', about: 'A narration whose trimmed text starts with a lone low half after thinking that ends in a lone high one.',
    events: [reasoning('QA \ud83d'), preamble(' \ude00 ')] },
  { name: 'reasoning_cap_splits_pair', about: 'The newest 65536 units of reasoning start inside a pair: Node keeps the lone low half.',
    events: [reasoning(`${E}${a(65535)}`), delta('QA')] },
  { name: 'routing_detail_valid', about: 'check-unavailable keeps a valid detail code.',
    events: [meta({ routing: { route: 'cloud', reason: 'check-unavailable', detail: `q${'a-9'.repeat(13)}` } }), delta('QA')] },
  { name: 'routing_detail_too_long', about: 'A 41-unit detail is dropped.', events: [meta({ routing: { route: 'local', reason: 'check-unavailable', detail: `q${a(40)}` } })] },
  { name: 'routing_detail_bad_shapes', about: 'Uppercase, a digit first, a newline at the end, a number: dropped; the last meta wins.',
    events: [meta({ routing: { route: 'local', reason: 'check-unavailable', detail: 'Qa' } }), meta({ routing: { route: 'local', reason: 'check-unavailable', detail: '9qa' } }),
      meta({ routing: { route: 'local', reason: 'check-unavailable', detail: 'qa\n' } }), meta({ routing: { route: 'cloud', reason: 'check-unavailable', detail: 5 } })] },
  { name: 'routing_detail_other_reason', about: 'A detail with another reason is not kept.', events: [meta({ routing: { route: 'local', reason: 'auto', detail: 'qa-timeout' } })] },
  { name: 'routing_rejected', about: 'A route other than local/cloud, a non-string reason, a non-object routing: no target.',
    events: [meta({ routing: { route: 'qa', reason: 'auto' } }), meta({ routing: { route: 'local', reason: 3 } }), meta({ routing: 'local' }), meta({ routing: ['local'] })] },
  { name: 'route_kinds', about: 'The route label is String(route).', events: [meta({ route: [1.5, null, [true, 1e21]], routingDecision: 'qa-decision' }), delta('QA')] },
  { name: 'route_number', about: 'A number route.', events: [meta({ route: 1e-7, routingDecision: 0 }), delta('QA')] },
  { name: 'route_falsy', about: 'Falsy routes change nothing; a later meta without routingDecision clears it.',
    events: [meta({ route: 'fast', routingDecision: { qa: 1 } }), meta({ route: '' }), meta({ route: 0 }), meta({ route: null }), meta({ route: false }), meta({ route: true }), delta('QA')] },
  { name: 'route_object', about: 'An object route is [object Object].', events: [meta({ route: {}, routingDecision: [] }), delta('QA')] },
  { name: 'route_throws', about: 'A route with its own toString: observe throws, nothing of that meta is kept.',
    events: [meta({ route: { toString: 'qa' }, routingDecision: { qa: 1 }, routing: { route: 'local', reason: 'auto' } }), meta({ route: [{ toString: 1 }] }), delta('QA')] },
  { name: 'stopped_route_target_kept', about: 'The placeholder keeps routeTarget and stats, not routingDecision, reasoning or sources.',
    clock: [T0, T0 + 10], events: [routed, { type: 'sources', sources: [{ title: 'QA' }] }, reasoning('QA'), usage()] },
  { name: 'sources_kinds', about: 'Only an array replaces the sources; an empty one is left out.',
    events: [{ type: 'sources', sources: [{ title: 'QA one' }] }, { type: 'sources', sources: 'qa' }, delta('QA'), { type: 'sources', sources: [] }] },
  { name: 'sources_kept', about: 'A non-array after an array keeps the array.', events: [{ type: 'sources', sources: [1, 'qa'] }, { type: 'sources' }, delta('QA')] },
  { name: 'usage_counts', about: 'count(): numbers only; elapsedMs from the clock at the usage event.',
    clock: [T0, T0 + 250, T0 + 900], events: [usage({ promptTokens: '3', completionTokens: null, totalTokens: 0, tokensPerSecond: 1.25 }), delta('QA'), usage({ promptTokens: 1e21, completionTokens: undefined })] },
  { name: 'usage_clock_backwards', about: 'A clock that went back: elapsedMs is 0.', clock: [T0 + 5000, T0], events: [usage(), delta('QA')] },
  { name: 'usage_empty', about: 'A usage with no counts keeps elapsedMs alone.', clock: [T0, T0 + 1], events: [{ type: 'usage' }, delta('QA')] },
  { name: 'tool_holes', about: 'Indexes leave holes; filter(Boolean) skips them.',
    events: [tool({ index: 3, name: 'qa_three' }), tool({ name: 'qa_append' }), tool({ index: 1, name: 'qa_one' }), delta('QA')] },
  { name: 'tool_bad_indexes', about: 'Negative, fractional, string and null indexes: appended.',
    events: [tool({ index: -1, name: 'qa_neg' }), tool({ index: 1.5, name: 'qa_frac' }), tool({ index: '0', name: 'qa_str' }), tool({ index: null, name: 'qa_null' }), tool({ index: 2.0, name: 'qa_two_float' })] },
  { name: 'tool_properties_past_array', about: 'Indexes 2^32 and 1e300 are properties, not elements: not kept.',
    events: [tool({ index: 2 ** 32, name: 'qa_prop' }), tool({ index: 1e300, name: 'qa_prop2' }), tool({ name: 'qa_after' }), result({ index: 2 ** 32, name: 'qa_prop' })] },
  { name: 'pending_without_index', about: 'tool_pending without an index takes the last slot (or 0).',
    events: [pending({ name: 'qa_first' }), tool({ index: 2, name: 'qa_two' }), pending({ name: 'qa_replaces_two', target: '', targetKind: 'folder' })] },
  { name: 'pending_target_kinds', about: 'target only when a non-empty string; targetKind only drive or drive-new.',
    events: [pending({ index: 0, target: 7, targetKind: 'drive' }), pending({ index: 1, target: 'QA file qa-file-3', targetKind: 'Drive' }), pending({ index: 2, targetKind: 'drive-new' })] },
  { name: 'result_by_name', about: 'A result without an index finds the first call of its name; none: appended.',
    events: [tool({ index: 0, name: 'qa_a' }), tool({ index: 2, name: 'qa_b' }), tool({ index: 3, name: 'qa_b' }), result({ name: 'qa_b', text: 'QA b' }), result({ name: 'qa_c', text: 'QA c' }), result({ name: 5, text: 'QA five' })] },
  { name: 'result_without_name', about: 'A result with no name keeps the previous name, args, target and kind.',
    events: [pending({ index: 0, name: 'qa_write', target: 'QA file qa-file-4', targetKind: 'drive' }), result({ index: 0, name: '', text: 'QA ok' }), result({ name: undefined, text: 'QA orphan' })] },
  { name: 'result_target_override', about: 'A result target replaces the card target; the kind stays.',
    events: [pending({ index: 0, target: 'QA old', targetKind: 'drive-new' }), result({ index: 0, name: 'qa_write', target: 'QA new' })] },
  { name: 'result_before_call', about: 'A result for a call never announced.', events: [result({ index: 1, name: 'qa_late', text: 7 })] },
  { name: 'result_flags', about: 'declined wins over notRun; only true counts.',
    events: [result({ index: 0, declined: true, notRun: true }), result({ index: 1, notRun: true }), result({ index: 2, declined: 'true', notRun: 1 }), result({ index: 3, declined: false })] },
  { name: 'result_cap_exact', about: 'A result of exactly 4000 units, and 4001.', events: [result({ index: 0, text: a(4000) }), result({ index: 1, text: a(4001) })] },
  { name: 'result_cap_pair_fits', about: 'An astral character ending at unit 4000 is kept whole.', events: [result({ index: 0, text: `${a(3998)}${E}b` })] },
  { name: 'result_cap_splits_pair', about: 'The cut at 4000 falls inside a pair (Node keeps the lone high surrogate).', events: [result({ index: 0, text: `${a(3999)}${E}b` })] },
  { name: 'result_cap_astral_only', about: 'Astral-only text cut at 4000 units: 2000 characters.', events: [result({ index: 0, text: E.repeat(2001) })] },
  { name: 'content_at_cap', about: 'Content of exactly 65536 units: not truncated.', events: [delta(a(65536))] },
  { name: 'content_over_cap', about: 'Content of 65537 units: the newest 65536, truncated.', events: [delta('b'), delta(a(65536))] },
  { name: 'content_cap_splits_pair', about: 'The newest 65536 units start inside a pair (Node keeps the lone low surrogate).', events: [delta(`${E}${a(65535)}`)] },
  { name: 'content_cap_pair_boundary', about: 'The cut falls between two pairs.', events: [delta(`${E}${E}${a(65534)}`)] },
  { name: 'reasoning_over_cap', about: 'Reasoning is cut too, but never marks truncated.', events: [reasoning(`${E}${a(65536)}`), delta('QA')] },
  { name: 'stopped_reasoning_over_cap', about: 'The placeholder keeps long thinking in the partial; it is cut.', events: [reasoning(`${a(65537)}`)] },
  { name: 'saved_at_now', about: 'savedAt is Date.now() when the file is written.', now: T0 + 123456, events: [delta('QA saved later')] },
];

// ── Seeded sequences ────────────────────────────────────────────────────────────────────────
let seed = 0x5eed2026;
const rand = () => { seed |= 0; seed = (seed + 0x6d2b79f5) | 0; let t = Math.imul(seed ^ (seed >>> 15), 1 | seed); t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t; return ((t ^ (t >>> 14)) >>> 0) / 4294967296; };
const pick = (list) => list[Math.floor(rand() * list.length)];
const maybe = (p, value) => (rand() < p ? value : undefined);
const INDEXES = [0, 0, 1, 1, 2, 3, 5, -1, 1.5, '1', null, undefined, undefined, 2 ** 32];
const NAMES = ['qa_read', 'qa_write', 'drive_trash_file', '', 7, undefined];
const TEXTS = ['\ud83d', '\ude00', 'QA answer.', ' ', '\n', 'QA tail', '', 5, E, '﻿QA', '\u0085', 'QA let me check.'];
const NARRATION = ['\ud83d', '\ude00', 'QA let me check.', '  QA spaced narration  ', 'QA answer.', E, '', '\u0085', '　', 'QA tail'];
const FLAGS = [true, true, false, 'true', 1, undefined, undefined];
const DECLINED = ['qa_a', 'qa_b', 'qa_a', 'qa bad', a(80), a(81), 'qa.c-d', '', 7, 'é', 'qa_1', 'qa_2', 'qa_3', 'qa_4', 'qa_5', 'qa_6', 'qa_7', 'qa_8', 'qa_9'];
const MAKERS = [
  () => meta({ route: pick(['fast', 'smart', '', 0, 1, true, ['qa', null], {}, null, undefined, 1e21]), routingDecision: pick([{ role: 'fast' }, null, 0, 'qa', undefined]),
    routing: pick([{ route: pick(['local', 'cloud', 'qa']), reason: pick(['check-unavailable', 'auto', 3]), detail: pick(['qa-timeout', 'Qa', a(40), `q${a(40)}`, '9qa', 'q', 5, undefined]) }, 'local', null, undefined]) }),
  () => ({ type: 'sources', sources: pick([[{ title: 'QA source' }], [], 'qa', undefined]) }),
  () => reasoning(pick(['QA thinking.', '', ' ', null, `${E} QA`])),
  () => preamble(pick(NARRATION)),
  () => delta(pick(TEXTS)), () => delta(pick(TEXTS)), () => delta(pick(TEXTS)),
  () => tool({ index: pick(INDEXES), name: pick(NAMES), args: pick(['{}', '', 3]) }),
  () => pending({ index: pick(INDEXES), name: pick(NAMES), target: pick(['QA file qa-file-1', '', 9, undefined]), targetKind: pick(['drive', 'drive-new', 'folder', undefined]) }),
  () => result({ index: pick(INDEXES), name: pick(NAMES), text: pick(['QA result', '', 42, `${a(3999)}${E}`, `${a(3998)}${E}`, E.repeat(2001)]),
    declined: pick(FLAGS.concat([undefined, undefined])), notRun: pick(FLAGS.concat([undefined, undefined])), applied: pick(FLAGS), target: pick(['QA target', '', undefined, undefined]) }),
  () => result({ index: pick(INDEXES), name: pick(NAMES), applied: true }),
  () => paused({ reason: pick(['declined', 'declined', 'sensitive-tool-result', 'supervision', 'qa-other', undefined, 5]), applied: pick([0, 1, 3, -1, 1.5, '2', undefined]),
    declined: rand() < 0.2 ? 'qa_a' : DECLINED.filter(() => rand() < 0.5) }),
  () => usage({ promptTokens: pick([12, 0, 1.5, '3', null, undefined]), completionTokens: pick([34, undefined]), tokensPerSecond: pick([7.25, 'fast', undefined]) }),
  () => (rand() < 0.5 ? error : pick([{ type: 'done' }, { type: 'telemetry' }, null, 'delta', { type: 7 }, []])),
];
for (let n = 0; n < 240; n++) {
  const events = [];
  const length = 1 + Math.floor(rand() * 14);
  for (let i = 0; i < length; i++) events.push(pick(MAKERS)());
  const start = T0 + Math.floor(rand() * 1000);
  const clock = [start];
  for (const ev of events) if (ev && ev.type === 'usage') clock.push(start + Math.floor(rand() * 4000) - 500);
  CASES.push({ name: `random_${String(n).padStart(3, '0')}`, about: 'A seeded sequence from the vocabulary.', clock, events,
    now: maybe(0.3, T0 + Math.floor(rand() * 100000)) });
}

const BOUNDED = [
  { name: 'entry_plain', about: 'A short partial entry.', entry: { role: 'assistant', content: 'QA', interrupted: true } },
  { name: 'entry_no_content', about: 'No content: an empty string is added.', entry: { role: 'assistant', model: 'Stopped' } },
  { name: 'entry_null_content', about: 'null content: an empty string.', entry: { content: null, reasoning: 'QA' } },
  { name: 'entry_number_content', about: 'A number content is kept; no length.', entry: { content: 5, reasoning: 7 } },
  { name: 'entry_object_content', about: 'Objects and arrays pass through.', entry: { content: { qa: 1 }, reasoning: ['qa'] } },
  { name: 'entry_array_content', about: 'A short array content: no length over the cap.', entry: { content: ['qa', 'b'] } },
  { name: 'entry_false_content', about: 'false content is kept (only null and undefined are replaced).', entry: { content: false } },
  { name: 'entry_saved_at_replaced', about: 'An old savedAt is replaced.', entry: { savedAt: 1, content: 'QA', truncated: false } },
  { name: 'entry_truncated_kept', about: 'A truncated key stays when the content is short.', entry: { content: 'QA', truncated: 'qa' } },
  { name: 'entry_long_both', about: 'Both fields over the cap.', entry: { content: a(70000), reasoning: `${E}${a(65536)}`, interrupted: false } },
  { name: 'entry_long_reasoning_only', about: 'Reasoning over the cap does not mark truncated.', entry: { content: '', reasoning: a(65537) } },
  { name: 'entry_object_length', about: 'An object content with its own number length over the cap is truncated.', entry: { content: { length: 70000 } } },
  { name: 'entry_object_length_small', about: 'An own length under the cap.', entry: { content: { length: 3, qa: 'x' } } },
  { name: 'entry_numbers_as_written', about: 'Numbers in a passed-through key, as written.', entry: raw('{"content":"QA","stats":{"elapsedMs":-0,"promptTokens":1e2,"tokensPerSecond":2.50}}') },
  { name: 'entry_index_keys', about: 'Array-index keys come first in JS property order.', entry: raw('{"role":"assistant","2":"b","content":"QA","1":"a"}') },
  { name: 'entry_lone_surrogates', about: 'Lone halves in content and reasoning, short of the cap.', entry: { content: '\ud83dQA', reasoning: 'QA\ude00' } },
  { name: 'entry_running_marker_kept', about: 'Other keys, even running, pass through.', entry: { content: 'QA', running: { start: 'qa' }, toolCalls: [] } },
];

(async () => {
  const names = new Set();
  for (const c of [...CASES, ...BOUNDED]) { if (names.has(c.name)) throw new Error(`duplicate case ${c.name}`); names.add(c.name); }
  const fixture = {
    version: 1,
    generator: 'crates/chat-turn/tests/record_entries.cjs (noevia-core chat-interrupted-turn.cjs createTurnRecord, chat-partial.cjs bounded and finish)',
    fixedNow: new Date(FIXED_NOW).toISOString(),
    sources: SOURCES,
    partialCapChars: partials.PARTIAL_CAP_CHARS,
    cases: CASES.map(run),
    boundedCases: BOUNDED.map((c) => runBounded({ now: FIXED_NOW + 42, ...c })),
  };
  fs.rmSync(workDir, { recursive: true, force: true });
  const text = `${JSON.stringify(fixture, null, 1)}\n`;
  const check = process.argv.indexOf('--check');
  if (check > 0) {
    const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
    if (committed !== text) { process.stderr.write('the recording differs from the committed fixture: record it again\n'); process.exit(1); }
    process.stderr.write(`ok: ${fixture.cases.length} records and ${fixture.boundedCases.length} bounded entries recorded as committed\n`);
  } else process.stdout.write(text);
  setImmediate(() => process.exit(0));
})().catch((error) => { fs.rmSync(workDir, { recursive: true, force: true }); process.stderr.write(`${error?.stack || error}\n`); process.exit(1); });
