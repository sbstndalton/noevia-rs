#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own toolbox catalogue (toolboxes.cjs createToolboxes:
// allToolboxes, sanitizeToolboxes, builtinToolboxes; mcp-servers.cjs parseEnabledToolboxes and
// createToolboxOffered) and project-catalogue.cjs catalogueOf on synthetic toolbox sets, and records
// per case what core publishes as project-catalogue.json, next to the inputs the Rust front composes
// the same catalogue from: the facts document core writes as toolbox-chat-facts.json
// (toolbox-chat-facts.cjs buildFacts) and the discovered MCP boxes. Track R14 step 4.
//
//   DAV_PARSE_WASM=<dav-parse.wasm whose sha256 is core's server/dav-parse.lock pin> \
//   node tools/gen-project-catalogue-fixtures.cjs <noevia-core checkout> <output.json>
//   ... node tools/gen-project-catalogue-fixtures.cjs <noevia-core checkout> --check <fixture.json>
//     records again and fails unless the result is byte-identical to the committed file.
// ENABLED_TOOLBOXES is parsed and judged by core's wasm port (mcp-servers.cjs): build the module from the
// lock's NOEVIA_RS_REF with tools/build-dav-parse-wasm.sh. Core checks its sha against the lock and a
// fault answers "not offered", so the recorder refuses to run when the module does not load.
//
// The `current()` handed to createCataloguePublisher lives in core server/index.cjs (lines 531-539 at
// the recorded ref), which cannot be loaded on its own: it is transcribed below (CURRENT) and the
// file's sha256 is recorded with the loaded ones, so a change to it needs a fresh look.
// bins/noevia-server/src/project_catalogue.rs replays every case. Nothing from node_modules is loaded
// (checked); each core file loaded is listed with its sha256.
'use strict';
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const core = path.resolve(process.argv[2] || '');
const checkAt = process.argv[3] === '--check' ? process.argv[4] : null;
const out = checkAt ? null : process.argv[3];
if (!process.argv[2] || (!out && !checkAt)) throw Error('usage: gen-project-catalogue-fixtures.cjs <core> <output.json> | --check <fixture.json>');
// A warning or error is a fault in core's port (mcp-servers.cjs answers "not offered" then): refuse to record.
const faults = [];
for (const k of ['log', 'info', 'debug']) console[k] = () => {};
for (const k of ['warn', 'error']) console[k] = (...a) => faults.push(a.map(String).join(' '));
const req = (m) => require(path.join(core, 'server', m));
const { createToolboxes } = req('toolboxes.cjs');
const { parseEnabledToolboxes, createToolboxOffered } = req('mcp-servers.cjs');
const { buildFacts } = req('toolbox-chat-facts.cjs');
const { catalogueOf } = req('project-catalogue.cjs');
if (!process.env.DAV_PARSE_WASM) throw Error('DAV_PARSE_WASM must name the pinned dav-parse.wasm');
req('dav-parse-wasm.cjs').load();
const ICONS = require(path.join(core, 'contracts', 'project-icons.json'));
const TRANSCRIBED = ['server/index.cjs'];

// core index.cjs projectCatalogue's `current`, over one createToolboxes result.
const CURRENT = (t) => ({
  selectable: t.sanitizeToolboxes(t.allToolboxes().map((b) => b.id)) || [],
  known: t.allToolboxes().map((b) => b.id),
  defaults: t.DEFAULT_TOOLBOXES,
  icons: Object.keys(ICONS),
});

// ── Synthetic boxes (QA-named) ──────────────────────────────────────────────
const tool = (name, description = `QA ${name}`) => ({ type: 'function', function: { name, description, parameters: { type: 'object', properties: {}, required: [] } } });
const KIWIX = { id: 'kiwix', label: 'QA Offline Wikipedia', description: 'QA offline copy.', source: 'builtin', tools: [tool('qa_kiwix_search')], reads: ['qa_kiwix_search'] };
const DRIVE = { id: 'gdrive', label: 'QA Drive', description: 'QA drive box.', source: 'builtin', inApp: true, tools: [tool('qa_drive_list'), tool('qa_drive_write')], reads: ['qa_drive_list'] };
const mcp = (id, server = 'qa-mcp', tools = [tool(`qa_${id}_read`)]) => ({ id, label: `QA ${id}`, description: `QA box ${id}`, source: 'mcp', server, tools, reads: tools.slice(0, 1).map((t) => t.function.name) });

const cases = [
  { name: 'core-only', builtin: [], mcp: [], enabled: null },
  { name: 'kiwix-drive', builtin: [KIWIX, DRIVE], mcp: [], enabled: null },
  { name: 'mcp-boxes', builtin: [KIWIX, DRIVE], mcp: [mcp('qa-notes'), mcp('qa-calendar'), mcp('qa-deck', 'qa-mcp-2')], enabled: null },
  { name: 'enabled-filters-mcp', builtin: [KIWIX, DRIVE], mcp: [mcp('qa-notes'), mcp('qa-calendar')], enabled: 'core,qa-notes' },
  // The connector box stays known (resolvable) when the operator's list leaves it out; never selectable.
  { name: 'enabled-omits-connector', builtin: [KIWIX, DRIVE], mcp: [mcp('qa-notes')], enabled: 'core,kiwix,qa-notes' },
  { name: 'enabled-omits-core', builtin: [KIWIX], mcp: [mcp('qa-notes')], enabled: 'kiwix,qa-notes' },
  { name: 'enabled-spaces-case', builtin: [KIWIX], mcp: [mcp('qa-notes'), mcp('QA-Upper')], enabled: ' core , QA-Upper ,,qa-notes ' },
  { name: 'enabled-empty', builtin: [KIWIX, DRIVE], mcp: [mcp('qa-notes')], enabled: '' },
  // Directory boxes (dir-*) are offered whatever the operator's list says.
  { name: 'directory-boxes', builtin: [KIWIX], mcp: [mcp('dir-qa-alpha', 'qa-dir'), mcp('qa-notes')], enabled: 'core' },
  { name: 'enabled-unknown-only', builtin: [KIWIX], mcp: [mcp('qa-notes')], enabled: 'qa-nothing' },
  // An MCP box with a built-in's id (or twice the same id): `known` keeps both, `selectable` once.
  { name: 'duplicate-ids', builtin: [KIWIX], mcp: [mcp('kiwix', 'qa-mcp-dup'), mcp('qa-notes'), mcp('qa-notes', 'qa-mcp-2')], enabled: null },
  { name: 'mcp-named-like-connector', builtin: [KIWIX], mcp: [mcp('gdrive', 'qa-mcp-drive')], enabled: null },
  { name: 'unicode-ids', builtin: [], mcp: [mcp('qa-naïve'), mcp('qa-\u{1f600}-box'), mcp('qa-ß')], enabled: null },
  { name: 'many-mcp', builtin: [KIWIX, DRIVE], mcp: Array.from({ length: 30 }, (_, i) => mcp(`qa-box-${String(i).padStart(2, '0')}`, `qa-mcp-${i % 3}`)), enabled: null },
];

function run(c) {
  const enabled = parseEnabledToolboxes(c.enabled === null ? {} : { ENABLED_TOOLBOXES: c.enabled });
  const offered = createToolboxOffered(enabled);
  const t = createToolboxes({
    boxes: c.builtin, mcpBoxes: () => c.mcp, offered,
    prefill: { budgetFor: () => null, rateFor: () => 0, stats: () => [] },
  });
  const facts = buildFacts({
    builtin: () => t.builtinToolboxes(), defaultToolboxes: t.DEFAULT_TOOLBOXES, connectorBoxes: t.CONNECTOR_BOXES,
    harnessEnabled: () => false, hasRepositories: () => false, prefill: { targetMs: 14000, stats: () => [] },
  });
  return { facts, catalogue: JSON.stringify(catalogueOf(CURRENT(t))) };
}

function sha(file) { return crypto.createHash('sha256').update(fs.readFileSync(path.join(core, file))).digest('hex'); }

function sources() {
  const all = Object.keys(require.cache);
  if (all.some((k) => k.includes(`${path.sep}node_modules${path.sep}`))) throw Error('a module from node_modules was loaded');
  const loaded = all.filter((k) => k.startsWith(core + path.sep)).map((k) => path.relative(core, k).split(path.sep).join('/'));
  return Object.fromEntries([...loaded, ...TRANSCRIBED].sort().map((f) => [f, sha(f)]));
}

const recorded = cases.map((c) => ({ name: c.name, enabled: c.enabled, mcpBoxes: c.mcp, node: run(c) }));
if (faults.length) { process.stderr.write(`core reported faults:\n${faults.join('\n')}\n`); process.exit(1); }
const wasm = crypto.createHash('sha256').update(fs.readFileSync(process.env.DAV_PARSE_WASM)).digest('hex');
const doc = { version: 1, generator: 'tools/gen-project-catalogue-fixtures.cjs', wasmSha256: wasm, icons: Object.keys(ICONS), sourcesRead: sources(), cases: recorded };
const text = `${JSON.stringify(doc, null, 2)}\n`;
if (checkAt) {
  if (fs.readFileSync(checkAt, 'utf8') !== text) { process.stderr.write(`${checkAt} differs from a fresh recording\n`); process.exit(1); }
  process.stderr.write(`${checkAt}: identical\n`);
} else fs.writeFileSync(out, text);
