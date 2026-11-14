'use strict';
// Records core's server/toolboxes.cjs executeToolCall (runToolCall's routing, argument checks and failure
// rule) for the Rust port (crates/tool-dispatch). Synthetic inputs only, deterministic (a seeded generator);
// nothing is contacted. Every executor is a fake that answers with a marker naming itself and the
// arguments it was given, so a recording shows which executor ran and with what:
//
//   specialist  -> SPEC <JSON.stringify(args)>
//   routed/discovered MCP -> MCP <serverId|-> <JSON.stringify(args)>   (mcp_fail* names: ERROR from tool: …)
//   kiwix       -> KIWIX <name> <JSON.stringify(args)>
//   drive       -> DRIVE <name> <JSON.stringify(args)>                 (drive_fail* names: ERROR: …)
//   clock       -> Current time: <ZONED|SERVER>[ (<zone>)] | ISO: ISO | FACTS   (zones starting "Bad" throw)
//   file read   -> ERROR: RPF <JSON.stringify([args.name])>
//
// A call whose name contains "write" sends through tool-write-fence.cjs; the case's fence is closed when
// `fence` is true.
//
//   node crates/tool-dispatch/tests/gen-fixtures.cjs <core-checkout> <out.json>
//
// The fixture carries the sha256 of the source file it was recorded from and the core commit given in
// NOEVIA_CORE_REF (if set).
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');

const [core, out] = process.argv.slice(2);
if (!core || !out) throw new Error('Usage: node gen-fixtures.cjs <core-checkout> <out.json>');
const server = path.join(path.resolve(core), 'server');
const file = path.join(server, 'toolboxes.cjs');

// The built-ins' collaborators, replaced before toolboxes.cjs requires them lazily.
const stub = (name, exports) => {
  const key = path.join(server, name);
  require.cache[key] = { id: key, filename: key, loaded: true, exports, children: [], paths: [] };
};
stub('project-file-names.cjs', { resolveProjectFile: (_project, name) => ({ error: `RPF ${JSON.stringify([name])}` }) });
stub('current-time-part.cjs', { factsLine: () => 'FACTS', calendarFacts: () => null, serverTimeZone: () => 'UTC' });
const RealDTF = Intl.DateTimeFormat;
Intl.DateTimeFormat = function DateTimeFormat(locale, opts) {
  if (typeof opts?.timeZone === 'string' && opts.timeZone.startsWith('Bad')) throw new RangeError('Invalid time zone specified');
  return { format: () => 'ZONED' };
};
const RealDate = Date;
globalThis.Date = class extends RealDate {
  toString() { return 'SERVER'; }
  toISOString() { return 'ISO'; }
};

const { createToolboxes } = require(file);
const fence = require(path.join(server, 'tool-write-fence.cjs'));

let seed = 0x7001d15;
const rand = () => { seed = (seed * 1103515245 + 12345) >>> 0; return seed / 2 ** 32; };
const pick = (xs) => xs[Math.floor(rand() * xs.length)];
const some = (xs, p = 0.5) => xs.filter(() => rand() < p);

const NAMES = ['get_current_time', 'read_project_file', 'page_tool', 'kw_search', 'kw_write', 'drive_list', 'drive_write',
  'drive_fail', 'mcp_a', 'mcp_write', 'mcp_fail', 'mcp_shared', 'unknown_x', 'Get_Current_Time', 'get_current_time ',
  'tool😀', 'kw_search\u0000'];
const SERVERS = ['srv1', 'srv2', 'directory_x', '', 'srv😀'];
const big = 'x'.repeat(198) + '😀' + 'y'.repeat(40);
const ARGS = ['', '{}', ' {} ', '\n{}\t', '{"timezone":"Europe/Berlin"}', '{"timezone":""}', '{"timezone":5}', '{"timezone":null}',
  '{"timezone":"Bad/Zone"}', JSON.stringify({ timezone: 'Bad' + 'Z'.repeat(100) }), JSON.stringify({ timezone: 'Bad' + '😀'.repeat(40) }),
  '{"timezone":"Asia/Tokyo","timezone":"UTC"}', 'null', '42', '"x"', '[]', '[1,2]', 'true', 'false', '0', '""', 'not json', '{',
  '{"a":}', '﻿{}', '{"a":1e400}', '{"a":-1e400}', '{"a":"\\ud83d"}', '{"a":"\\udc00x"}', '{"b":1,"a":2,"1":3,"0":4}',
  '{"name":"x.md","offset":3}', '{"name":"Folder/Text/notes.md","startPage":1,"endPage":2}', '{"name":5}', '{"__proto__":1}',
  '{"a":1,"a":2}', '{"x":-0}', '{"n":1.0000000000000002}', '{"n":1e21}', '{"n":1e-7}', '{"n":123456789012345680000}',
  '{"s":"\\u2028\\u2029"}', '{"s":"\\u0000"}', '{"nested":{"z":1,"a":[1,{"q":null}]}}', big, 'x'.repeat(250), '😀'.repeat(150),
  '{' + '"a":'.repeat(0) + '"k":"' + '😀'.repeat(120) + '"}', '['.repeat(150) + ']'.repeat(150),
  '{"d":' + '['.repeat(140) + ']'.repeat(140) + '}', 'NaN', 'undefined', "{'a':1}", '{"a":1,}'];

function caseFor(name, rawArgs, cfg) {
  return { name, rawArgs, ...cfg };
}

async function run(c) {
  const t = createToolboxes({
    scope: { run: (_s, fn) => fn(), getStore: () => ({ authn: { user: { id: 'qa-user' } } }) },
    kiwixTools: c.kiwix ? { names: new Set(c.kiwix), execute: async (name, args) => (name.includes('write') ? fence.send(() => `KIWIX ${name} ${JSON.stringify(args)}`) : `KIWIX ${name} ${JSON.stringify(args)}`) } : null,
    driveTools: c.drive ? { names: new Set(c.drive), execute: async (_u, name, args) => (name.startsWith('drive_fail') ? `ERROR: DRIVE ${name}` : name.includes('write') ? fence.send(() => `DRIVE ${name} ${JSON.stringify(args)}`) : `DRIVE ${name} ${JSON.stringify(args)}`) } : null,
    mcpTools: () => new Map((c.mcp || []).map((n) => [n, {}])),
    executeMcp: async (name, args, _signal, serverId) => {
      const marker = `MCP ${serverId === undefined ? '-' : serverId} ${JSON.stringify(args)}`;
      if (name.startsWith('mcp_fail')) return `ERROR from tool: ${marker}`;
      return name.includes('write') ? fence.send(() => marker) : marker;
    },
    specialistTools: c.specialist ? { NAME: 'page_tool', appliesTo: (p) => p?.coworker === true, execute: async (_p, args) => `SPEC ${JSON.stringify(args)}` } : null,
  });
  const project = c.project === null ? null : { id: 'qa-project', files: [], coworker: c.project === 'coworker' };
  const outcome = {};
  const text = await t.executeToolCall(project, c.name, c.rawArgs, c.allowed ? new Set(c.allowed) : undefined, undefined, outcome,
    { ...(c.routes ? { routes: new Map(c.routes) } : {}), ...(c.fence ? { writeFence: { open: () => false, started: () => {} } } : {}) });
  return { text: String(text ?? ''), failed: outcome.failed === true, notRun: outcome.notRun === true };
}

function config() {
  return {
    allowed: rand() < 0.75 ? some(NAMES, 0.7) : null,
    routes: rand() < 0.6 ? some(NAMES, 0.3).map((n) => [n, pick(SERVERS)]).concat(rand() < 0.2 ? [[pick(NAMES), pick(SERVERS)]] : []) : null,
    project: pick([null, 'plain', 'coworker']),
    specialist: rand() < 0.6,
    kiwix: rand() < 0.6 ? ['kw_search', 'kw_write', ...(rand() < 0.2 ? ['get_current_time'] : [])] : null,
    drive: rand() < 0.6 ? ['drive_list', 'drive_write', 'drive_fail', ...(rand() < 0.2 ? ['kw_search', 'read_project_file'] : [])] : null,
    mcp: some(['mcp_a', 'mcp_write', 'mcp_fail', 'mcp_shared', 'page_tool', 'get_current_time', 'kw_search', 'drive_list'], 0.6),
    fence: rand() < 0.25,
  };
}

(async () => {
  const cases = [];
  // Every name with every argument text under one permissive configuration, then random ones.
  const open = { allowed: null, routes: null, project: 'coworker', specialist: true, kiwix: ['kw_search', 'kw_write'],
    drive: ['drive_list', 'drive_write', 'drive_fail'], mcp: ['mcp_a', 'mcp_write', 'mcp_fail', 'mcp_shared'], fence: false };
  for (const name of NAMES) for (const rawArgs of ARGS) cases.push(caseFor(name, rawArgs, open));
  for (let i = 0; i < 2500; i++) cases.push(caseFor(pick(NAMES), pick(ARGS.slice(0, 12).concat(['{}', '{}', '{"timezone":"UTC"}'])), config()));
  for (const c of cases) Object.assign(c, await run(c));
  const src = fs.readFileSync(file);
  const fixture = {
    version: 1,
    generator: 'noevia-rs crates/tool-dispatch/tests/gen-fixtures.cjs (from noevia-core server/toolboxes.cjs)',
    source: { path: 'server/toolboxes.cjs', sha256: crypto.createHash('sha256').update(src).digest('hex'), coreRef: process.env.NOEVIA_CORE_REF || null },
    fenceMessage: (() => { try { fence.run({ open: () => false }, () => fence.send(() => null)); } catch (e) { return e.message; } return null; })(),
    resultCap: require(file).TOOL_RESULT_CAP,
    cases,
  };
  Intl.DateTimeFormat = RealDTF;
  globalThis.Date = RealDate;
  fs.writeFileSync(out, JSON.stringify(fixture, null, 1) + '\n');
  console.log(`wrote ${cases.length} cases to ${out}`);
})().catch((e) => { console.error(e); process.exit(1); });
