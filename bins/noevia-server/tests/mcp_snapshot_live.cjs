'use strict';
// Node's half of the live snapshot test (tests/mcp_directory.rs `the_snapshot_node_writes_for_hostile_tools...`;
// sbstndalton/noevia#2202 round 2 review, H1). Core's REAL publisher (mcp-discovery-snapshot.cjs) writes
// `mcp-discovery.json` for a synthetic server that lists twenty thousand tools whose descriptions have an emoji
// across the 240-character cut, a name and an error likewise; the test reads the file with the front's own
// reader. Synthetic data only; nothing is contacted.
//
//   node mcp_snapshot_live.cjs        env: NOEVIA_MCP_CORE, DATA_DIR
const path = require('node:path');

const env = (name) => {
  const value = process.env[name];
  if (!value) { console.error(`missing ${name}`); process.exit(2); }
  return value;
};
const snapshot = require(path.join(env('NOEVIA_MCP_CORE'), 'server', 'mcp-discovery-snapshot.cjs'));

const emoji = '\u{1F600}';
const fn = (name, description) => ({ type: 'function', function: { name, description } });
const tools = Array.from({ length: 20000 }, (_, i) => fn(`tool_${i}`, `${'a'.repeat(239)}${emoji}${'b'.repeat(300)}`));
const box = { id: 'dir-hostile', server: 'dir-hostile', label: `${'L'.repeat(199)}${emoji}`, description: 'd', source: 'mcp', directory: true, tools };
const catalogue = snapshot.buildCatalogue({
  allToolboxes: () => [box], manifest: [], mcpEnabled: () => true, servers: [{ id: 'dir-hostile', url: 'https://x.example.test/mcp', auth: 'oauth', directory: true }],
  state: { servers: new Map([['dir-hostile', { error: `${'e'.repeat(999)}${emoji}`, toolCount: tools.length }]]), boxes: [box] },
  isWriteTool: () => false, harnessEnabled: () => false, hasRepositories: () => false, nextcloudOrigins: [], defaultToolboxes: [], connectorBoxes: [],
});
const publisher = snapshot.createMcpDiscoveryPublisher({ dataDir: env('DATA_DIR'), discoverMcpTools: async () => {}, catalogue: () => catalogue, everyMs: 600000, onError: (e) => { console.error(String(e && e.message)); process.exit(1); } });
publisher.publish().then(() => process.exit(0), (e) => { console.error(String((e && e.stack) || e).slice(0, 2000)); process.exit(1); });
