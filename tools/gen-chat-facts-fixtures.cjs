#!/usr/bin/env node
// Test-oracle glue only: builds noevia-core's toolbox chat facts (toolbox-chat-facts.cjs buildFacts, the
// document core publishes as toolbox-chat-facts.json) from core's own built-in boxes: toolboxes.cjs's
// core box (CORE_TOOLS), kiwix.cjs's Offline Wikipedia box (createKiwixTools(...).box) and
// gdrive-tools.cjs's Drive box (createDriveTools(...).box), offered through mcp-servers.cjs
// (ENABLED_TOOLBOXES) as core index.cjs wires them, under synthetic configurations. Track R14 step 7.
//
//   DAV_PARSE_WASM=<dav-parse.wasm whose sha256 is core's server/dav-parse.lock pin> \
//   node tools/gen-chat-facts-fixtures.cjs <noevia-core checkout> <output.json>
//   ... node tools/gen-chat-facts-fixtures.cjs <noevia-core checkout> --check <fixture.json>
//     records again and fails unless the result is byte-identical to the committed file.
// Both modes also write (or check) contracts/builtin-toolboxes.json next to the fixture's repository root:
// core's three built-in boxes as buildFacts publishes them (the front's copy of their tool definitions).
//
// The wiring lives in core server/index.cjs (lines 453-482: the kiwix box only with the construction-time
// `kiwix` feature and KIWIX_URL, the Drive box always; lines 1142-1149: buildFacts's inputs), which cannot
// be loaded on its own: it is transcribed in `run` below and the file's sha256 is recorded with the loaded
// ones. Nothing calls a network: the boxes are only constructed. bins/noevia-server/src/chat_facts_native.rs
// replays every case. Nothing from node_modules is loaded (checked); a fault core reports refuses the run.
'use strict';
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const core = path.resolve(process.argv[2] || '');
const checkAt = process.argv[3] === '--check' ? process.argv[4] : null;
const out = checkAt ? null : process.argv[3];
if (!process.argv[2] || (!out && !checkAt)) throw Error('usage: gen-chat-facts-fixtures.cjs <core> <output.json> | --check <fixture.json>');
const faults = [];
for (const k of ['log', 'info', 'debug']) console[k] = () => {};
for (const k of ['warn', 'error']) console[k] = (...a) => faults.push(a.map(String).join(' '));
if (!process.env.DAV_PARSE_WASM) throw Error('DAV_PARSE_WASM must name the pinned dav-parse.wasm');
const req = (m) => require(path.join(core, 'server', m));
req('dav-parse-wasm.cjs').load();
const { createToolboxes, TOOL_PREFILL_TARGET_MS, TOOL_RESULT_CAP } = req('toolboxes.cjs');
const { parseEnabledToolboxes, createToolboxOffered } = req('mcp-servers.cjs');
const { buildFacts } = req('toolbox-chat-facts.cjs');
const { createKiwixTools } = req('kiwix.cjs');
const { createDriveTools } = req('gdrive-tools.cjs');
const TRANSCRIBED = ['server/index.cjs'];

const cases = [
  { name: 'defaults', kiwix: false, kiwixUrl: null, enabled: null, harness: false, repos: false },
  { name: 'kiwix-on', kiwix: true, kiwixUrl: 'http://qa-kiwix.invalid:8080', enabled: null, harness: false, repos: false },
  // The construction-time feature off or no KIWIX_URL: no Offline Wikipedia box.
  { name: 'kiwix-feature-off', kiwix: false, kiwixUrl: 'http://qa-kiwix.invalid:8080', enabled: null, harness: false, repos: false },
  { name: 'kiwix-no-url', kiwix: true, kiwixUrl: null, enabled: null, harness: false, repos: false },
  { name: 'kiwix-empty-url', kiwix: true, kiwixUrl: '', enabled: null, harness: false, repos: false },
  { name: 'kiwix-path-url', kiwix: true, kiwixUrl: 'https://qa-kiwix.invalid/wiki/', enabled: null, harness: true, repos: true },
  // The operator's list: core always offered, the connector (Drive) box kept, kiwix only when listed.
  { name: 'enabled-core-only', kiwix: true, kiwixUrl: 'http://qa-kiwix.invalid', enabled: 'core', harness: false, repos: false },
  { name: 'enabled-kiwix', kiwix: true, kiwixUrl: 'http://qa-kiwix.invalid', enabled: 'offline-wikipedia', harness: false, repos: false },
  { name: 'enabled-blank', kiwix: true, kiwixUrl: 'http://qa-kiwix.invalid', enabled: '  ', harness: false, repos: false },
  { name: 'enabled-case', kiwix: true, kiwixUrl: 'http://qa-kiwix.invalid', enabled: 'Offline-Wikipedia, gdrive', harness: false, repos: false },
  { name: 'harness-on', kiwix: false, kiwixUrl: null, enabled: null, harness: true, repos: false },
  { name: 'repos-only', kiwix: false, kiwixUrl: null, enabled: null, harness: false, repos: true },
];

function run(c) {
  const offered = createToolboxOffered(parseEnabledToolboxes(c.enabled === null ? {} : { ENABLED_TOOLBOXES: c.enabled }));
  // index.cjs:453: `featureConstruction.kiwix && process.env.KIWIX_URL ? createKiwixTools(...) : null`.
  const kiwixTools = c.kiwix && c.kiwixUrl ? createKiwixTools({ baseUrl: c.kiwixUrl, cap: TOOL_RESULT_CAP, fetchImpl: () => { throw Error('no network in the recorder'); } }) : null;
  const driveTools = createDriveTools({ accounts: { forUser: () => { throw Error('no account in the recorder'); } }, cap: TOOL_RESULT_CAP });
  const t = createToolboxes({
    boxes: [kiwixTools && kiwixTools.box, driveTools.box].filter(Boolean), kiwixTools, driveTools,
    mcpBoxes: () => [], offered,
    prefill: { budgetFor: () => null, rateFor: () => 0, stats: () => ({}) },
  });
  // index.cjs:1144-1149 (prefill: this process's measurements, none in the recorder).
  return buildFacts({
    builtin: () => t.builtinToolboxes(), defaultToolboxes: t.DEFAULT_TOOLBOXES, connectorBoxes: t.CONNECTOR_BOXES,
    harnessEnabled: () => c.harness, hasRepositories: () => c.repos,
    prefill: { targetMs: TOOL_PREFILL_TARGET_MS, stats: () => ({}) },
  });
}

function sha(file) { return crypto.createHash('sha256').update(fs.readFileSync(path.join(core, file))).digest('hex'); }

function sources() {
  const all = Object.keys(require.cache);
  if (all.some((k) => k.includes(`${path.sep}node_modules${path.sep}`))) throw Error('a module from node_modules was loaded');
  const loaded = all.filter((k) => k.startsWith(core + path.sep)).map((k) => path.relative(core, k).split(path.sep).join('/'));
  return Object.fromEntries([...loaded, ...TRANSCRIBED].sort().map((f) => [f, sha(f)]));
}

const recorded = cases.map((c) => ({ ...c, facts: run(c) }));
if (faults.length) { process.stderr.write(`core reported faults:\n${faults.join('\n')}\n`); process.exit(1); }
const wasm = crypto.createHash('sha256').update(fs.readFileSync(process.env.DAV_PARSE_WASM)).digest('hex');
const doc = { version: 1, generator: 'tools/gen-chat-facts-fixtures.cjs', wasmSha256: wasm, sourcesRead: sources(), cases: recorded };
const text = `${JSON.stringify(doc, null, 2)}\n`;
// The boxes from the case that offers all three, in core's offer order.
const all = recorded.find((c) => c.name === 'kiwix-on').facts.builtin;
if (all.map((b) => b.id).join() !== 'core,offline-wikipedia,gdrive') throw Error(`unexpected built-in boxes ${all.map((b) => b.id)}`);
const contract = `${JSON.stringify({ version: 1, generator: 'tools/gen-chat-facts-fixtures.cjs', boxes: all }, null, 2)}\n`;
const contractFile = path.join(__dirname, '..', 'contracts', 'builtin-toolboxes.json');
if (checkAt) {
  if (fs.readFileSync(checkAt, 'utf8') !== text) { process.stderr.write(`${checkAt} differs from a fresh recording\n`); process.exit(1); }
  if (fs.readFileSync(contractFile, 'utf8') !== contract) { process.stderr.write(`${contractFile} differs from a fresh recording\n`); process.exit(1); }
  process.stderr.write(`${checkAt}, ${contractFile}: identical\n`);
} else { fs.writeFileSync(out, text); fs.writeFileSync(contractFile, contract); }
