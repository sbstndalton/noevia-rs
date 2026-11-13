'use strict';
// Node's half of track R5's live tests (tests/mcp_call.rs `node_hands_a_tool_call_to_the_real_front`,
// tests/gdrive_tools.rs `node_hands_a_drive_tool_to_the_real_front`). Run by those tests as a separate process
// against a noevia-core checkout that has the native paths (NOEVIA_R5_CORE; CI: the ref in
// tests/fixtures/r5-live.core-ref). It uses Core's REAL modules - the internal-store client
// (project-store-client.cjs), the MCP wiring (mcp-wiring.cjs) with its client of the front's discovery
// (mcp-native-discovery.cjs), and the Drive tools (gdrive-tools.cjs) - with their native switches on, against
// the in-process Rust front's real internal store listener and synthetic servers on 127.0.0.1. Synthetic data
// only: nothing outside this machine is contacted.
//
//   node r5_node_live.cjs <mcp|drive>      env: NOEVIA_R5_CORE, STORE_PORT, STORE_TOKEN, USER_ID
//
// Prints ONE line of JSON for the test to read.
const path = require('node:path');
const { AsyncLocalStorage } = require('node:async_hooks');

const env = (name) => {
  const value = process.env[name];
  if (!value) { console.error(`missing ${name}`); process.exit(2); }
  return value;
};
const core = (file) => require(path.join(env('NOEVIA_R5_CORE'), 'server', file));
const userId = env('USER_ID');
const broker = core('project-store-client.cjs').createClient({ port: Number(env('STORE_PORT')), token: env('STORE_TOKEN') });
const never = (what) => async () => { throw new Error(`${what} must not be used when the front runs the call`); };

async function mcpPhase() {
  const scope = new AsyncLocalStorage();
  const { createMcpWiring, createCredentialOriginCheck } = core('mcp-wiring.cjs');
  const wiring = createMcpWiring({
    servers: [{ id: 'open', url: 'http://127.0.0.1:9/never-used-by-node', auth: 'none' }],
    // Node's own transport must not run: the front makes the call.
    mcp: { connect: never('connect'), callTool: never('callTool'), disconnect: never('disconnect'), resultToText: () => '' },
    discovery: core('mcp-native-discovery.cjs').createNativeDiscovery({ broker }),
    directoryMcp: { signal: () => 0, asServers: () => [], boxFor: () => ({}), headersFor: () => ({}) },
    mcpOAuth: { connected: () => false, tokenFor: never('tokenFor'), session: never('session') },
    mcpKeys: { hasKey: () => false, headers: never('headers') },
    directoryUrlAllowed: async () => true,
    credentialOriginAllowed: createCredentialOriginCheck(''),
    scope,
    storageFor: () => ({ kind: 'local' }),
    isWriteTool: () => false,
    internal: { mintToken: () => 'unused' },
    internalKey: 'unused',
    reduceToolResult: (text, { maxChars }) => ({ text: `[reduced ${maxChars}] ${text}`, reduced: false }),
    env: {},
    logger: { log() {}, warn() {} },
    nativeCalls: { enabled: () => true, call: (payload, opts) => broker.mcpCall(payload, opts) },
  });
  const state = await wiring.discoverMcpTools(true);
  const asUser = (fn) => scope.run({ workspace: { userId }, authn: { user: { id: userId } } }, fn);
  return {
    tools: [...state.tools.keys()],
    text: await asUser(() => wiring.executeMcpToolCall('one', { q: 'qa é' }, undefined, 'open')),
    unknown: await asUser(() => wiring.executeMcpToolCall('nope', {}, undefined, 'open')),
  };
}

async function drivePhase() {
  const tools = core('gdrive-tools.cjs').createDriveTools({
    accounts: { forUser: () => { throw new Error('this process holds no Drive connection'); } },
    cap: 8000,
    native: { enabled: () => true, tool: (p) => broker.gdriveTool(p), target: (p) => broker.gdriveTarget(p) },
  });
  const user = { id: userId, role: 'member' };
  return {
    list: await tools.execute(user, 'drive_list_recent', {}, { exchangeKey: 'ex-live' }),
    updateBeforeRead: await tools.execute(user, 'drive_update_file', { fileId: 'f1', content: 'qa new' }, { exchangeKey: 'ex-live' }),
    read: await tools.execute(user, 'drive_read_file', { fileId: 'f1' }, { exchangeKey: 'ex-live' }),
    update: await tools.execute(user, 'drive_update_file', { fileId: 'f1', content: 'qa new' }, { exchangeKey: 'ex-live' }),
    target: await tools.describeTarget(user, 'drive_trash_file', '{"fileId":"f1"}'),
  };
}

const phase = process.argv[2];
const run = phase === 'mcp' ? mcpPhase : phase === 'drive' ? drivePhase : null;
if (!run) { console.error(`unknown phase ${phase}`); process.exit(2); }
run().then((out) => { process.stdout.write(`${JSON.stringify({ phase, ...out })}\n`); process.exit(0); },
  (error) => { console.error(String((error && error.stack) || error).slice(0, 2000)); process.exit(1); });
