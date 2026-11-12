'use strict';
// Node's half of the live chat-facts test (tests/mcp_directory.rs `the_facts_node_writes_...`; sbstndalton/noevia row T).
// Core's REAL publisher (toolbox-chat-facts.cjs) writes `toolbox-chat-facts.json` for synthetic hostile input: a model name
// and a note with a lone surrogate (JSON would write `\ud83d`, which the front's JSON reader refuses, making the file unreadable
// for every account), or a built-in box larger than the front's read limit (the publisher must refuse, not write). Synthetic only.
//
//   node toolbox_facts_live.cjs <surrogate|huge>      env: NOEVIA_MCP_CORE, DATA_DIR
const path = require('node:path');
const env = (name) => { const v = process.env[name]; if (!v) { console.error(`missing ${name}`); process.exit(2); } return v; };
const facts = require(path.join(env('NOEVIA_MCP_CORE'), 'server', 'toolbox-chat-facts.cjs'));
const fn = (name, description) => ({ type: 'function', function: { name, description, parameters: { type: 'object' } } });
const mode = process.argv[2];
const box = (description) => ({ id: 'core', label: 'Core', description, source: 'builtin', tools: [fn('get_current_time', `${'a'.repeat(239)}\ud83d`)], reads: ['get_current_time'] });
const build = () => facts.buildFacts({
  builtin: () => [box(mode === 'huge' ? 'x'.repeat(facts.MAX_BYTES) : 'ok')], defaultToolboxes: ['core'], connectorBoxes: ['gdrive'],
  harnessEnabled: () => false, hasRepositories: () => false, prefill: { targetMs: 1250, stats: () => ({ ['model\ud83d']: { note: 'n\udc00' } }) },
});
const publisher = facts.createChatFactsPublisher({ dataDir: env('DATA_DIR'), facts: build, everyMs: 600000, onError: (e) => { console.error(String(e && e.message)); process.exit(1); } });
publisher.publish().then(() => process.exit(0), (e) => { console.error(String((e && e.stack) || e).slice(0, 2000)); process.exit(1); });
