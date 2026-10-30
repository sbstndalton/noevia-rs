'use strict';
// Node's half of the live MCP credential test (tests/mcp_credentials.rs
// `node_signs_in_through_the_real_front_calls_a_tool_retries_a_401_and_disconnects`; sbstndalton/noevia#2202).
//
// Run by that test as a separate process against a paired noevia-core checkout (NOEVIA_MCP_CORE, the
// contracts/http/front-core.ref generation). It uses Core's REAL modules - the internal-store client
// (project-store-client.cjs), the broker adapter (rust-mcp-credentials.cjs), the MCP wiring
// (mcp-wiring.cjs) and the MCP protocol client (mcp.cjs) - against the in-process Rust front's real
// internal store listener and a synthetic MCP server on 127.0.0.1. Only a synthetic server and
// synthetic credentials: nothing outside this machine is contacted.
//
//   node mcp_node_live.cjs <phase>      env: NOEVIA_MCP_CORE, STORE_PORT, STORE_TOKEN, DATA_DIR, MCP_URL, USER_ID
//
// phases
//   call       discover the server's tools with the user's sign-in, then run one tool call as that user
//   after      what Node can see once the user has disconnected
//   directory  (sbstndalton/noevia#2202 round 2) the chat-time client over the REAL directory re-reader
//              (directory-mcp.cjs on the shared database): before the front adds a server, after (its tools
//              listed and one called, with the shared key the front sealed), and after the front removed it.
//              The test moves the stages by creating `go-added` and `go-removed` in DATA_DIR.
// Prints ONE line of JSON for the test to read. It never prints a token: it reports whether anything it
// saw looked like a refresh token instead.
const path = require('node:path');
const { AsyncLocalStorage } = require('node:async_hooks');

const env = (name) => {
  const value = process.env[name];
  if (!value) { console.error(`missing ${name}`); process.exit(2); }
  return value;
};
const server = path.join(env('NOEVIA_MCP_CORE'), 'server');
const core = (file) => require(path.join(server, file));
const userId = env('USER_ID');
const mcpUrl = env('MCP_URL');

const broker = core('project-store-client.cjs').createClient({ port: Number(env('STORE_PORT')), token: env('STORE_TOKEN') });
const credentials = core('rust-mcp-credentials.cjs').createMcpCredentials({ broker, dataDir: env('DATA_DIR') });
const { createMcpWiring, createCredentialOriginCheck } = core('mcp-wiring.cjs');

const scope = new AsyncLocalStorage();
const wiring = createMcpWiring({
  servers: [{ id: 'dir-live', url: mcpUrl, auth: 'oauth', title: 'Live', addedBy: userId }],
  manifest: [],
  mcp: core('mcp.cjs'),
  bindBoxes: ({ manifest }) => manifest.map(() => ({})),
  directoryMcp: { asServers: () => [], boxFor: () => ({}), headersFor: () => ({}) },
  mcpOAuth: credentials.oauth,
  mcpKeys: credentials.keys,
  directoryUrlAllowed: async () => true,
  credentialOriginAllowed: createCredentialOriginCheck(''),
  scope,
  storageFor: () => ({ kind: 'local' }),
  isWriteTool: () => false,
  internal: { mintToken: () => 'unused' },
  internalKey: 'unused',
  reduceToolResult: (text) => ({ text, reduced: false }),
  env: {},
  logger: { log() {}, warn() {} },
});

const asUser = (fn) => scope.run({ workspace: { userId }, authn: { user: { id: userId } } }, fn);

async function main(phase) {
  const out = { phase };
  if (phase === 'call') {
    // Who is ready comes from the file the front publishes, not from anything this process stored.
    out.status = credentials.oauth.status(userId, 'dir-live');
    const state = await wiring.discoverMcpTools(true);
    out.discoveryError = state.servers.get('dir-live')?.error ?? null;
    out.tools = [...state.tools.keys()];
    out.text = await asUser(() => wiring.executeMcpToolCall('live_echo', { text: 'hello' }));
    // What the broker hands this process for a call: an access token, never a refresh token.
    const session = await credentials.oauth.session(userId, 'dir-live');
    out.sessionKeys = Object.keys(session || {}).sort();
    out.sawRefreshToken = JSON.stringify(session).includes('refresh');
  } else if (phase === 'after') {
    out.status = credentials.oauth.status(userId, 'dir-live');
    out.connected = credentials.oauth.connected(userId, 'dir-live');
    out.session = await credentials.oauth.session(userId, 'dir-live');
  } else {
    console.error(`unknown phase ${phase}`);
    process.exit(2);
  }
  process.stdout.write(`${JSON.stringify(out)}\n`);
}

/** The directory phase: one process over the shared database, asked again after each change the front makes. */
async function directoryPhase() {
  const fs = require('node:fs');
  const Database = require(path.join(server, 'node_modules', 'better-sqlite3'));
  const db = new Database(path.join(env('DATA_DIR'), 'cowork.db'));
  const secrets = core('secrets.cjs').createSecretStore(env('DATA_DIR'), { env: {} });
  const directoryMcp = core('directory-mcp.cjs').createDirectoryMcp({ db, secrets });
  const w = createMcpWiring({
    servers: [], manifest: [], mcp: core('mcp.cjs'), bindBoxes: ({ manifest }) => manifest.map((m) => m), directoryMcp,
    mcpOAuth: credentials.oauth, mcpKeys: credentials.keys, directoryUrlAllowed: async () => true,
    credentialOriginAllowed: createCredentialOriginCheck(''), scope, storageFor: () => ({ kind: 'local' }), isWriteTool: () => false,
    internal: { mintToken: () => 'unused' }, internalKey: 'unused', reduceToolResult: (text) => ({ text, reduced: false }), env: {},
    logger: { log() {}, warn() {} },
  });
  const emit = (o) => process.stdout.write(`${JSON.stringify(o)}\n`);
  const waitFor = async (name) => {
    for (let i = 0; i < 600; i++) { if (fs.existsSync(path.join(env('DATA_DIR'), name))) return; await new Promise((r) => setTimeout(r, 50)); }
    throw new Error(`timed out waiting for ${name}`);
  };
  const stage = async (name, extra = {}) => {
    // An ordinary, unforced discovery: the re-reader is what notices the change.
    const state = await w.discoverMcpTools();
    emit({ stage: name, servers: w.servers.map((sv) => sv.id), auth: w.servers.map((sv) => sv.auth), tools: [...state.tools.keys()], boxes: state.boxes.map((b) => b.id), errors: [...state.servers.values()].map((sv) => sv.error), ...extra });
  };
  await stage('before');
  await waitFor('go-added');
  const state = await w.discoverMcpTools();
  const text = await asUser(() => w.executeMcpToolCall('echo', { text: 'hi' }));
  await stage('added', { text, toolCount: state.servers.get(w.servers[0]?.id)?.toolCount ?? null });
  await waitFor('go-removed');
  await stage('removed');
}

if (process.argv[2] === 'directory') {
  directoryPhase().then(() => process.exit(0), (error) => { console.error(String((error && error.stack) || error).slice(0, 2000)); process.exit(1); });
} else main(process.argv[2]).then(() => process.exit(0), (error) => { console.error(String((error && error.stack) || error).slice(0, 2000)); process.exit(1); });
