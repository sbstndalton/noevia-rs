'use strict';
// Test-only Node oracle for the front's native MCP tool call (track R5, mcp_call.rs). For every scenario it
// runs core's REAL executor (server/mcp-wiring.cjs createMcpWiring -> executeMcpToolCall, with core's real
// tool-rate-limit.cjs, mcp.cjs resultToText and mcp-internal.cjs mintToken) at the checkout given, with a
// scripted MCP transport, a scripted credential broker and a virtual clock, and records for every call: what
// the executor was asked (server, tool, arguments, caller, project, approval facts), the Nextcloud credential
// core's own mcpAuthHeaders gave for that scope, every transport round (address, headers, tool, arguments),
// every broker question (key, session with the refused token, reject with the generation) and the text it
// answered. Synthetic data only; nothing is contacted.
//
//   node bins/noevia-server/tests/mcp-call-oracle.cjs <core-checkout> <out.json>
//
// The output is deterministic (crypto.randomUUID and Date.now are fixed). tests/mcp_call_oracle.rs replays it
// through mcp_call::execute. The sha256 of every core file it loads is in the fixture.
process.env.TZ = 'UTC';
console.warn = () => {};
console.log = () => {};
const fs = require('node:fs');
const crypto = require('node:crypto');
const path = require('node:path');
const { AsyncLocalStorage } = require('node:async_hooks');

const [core, out] = process.argv.slice(2);
if (!core || !out) throw new Error('Usage: node mcp-call-oracle.cjs <core-checkout> <out.json>');
const SOURCES = ['mcp-wiring.cjs', 'mcp.cjs', 'tool-rate-limit.cjs', 'mcp-internal.cjs'];
const req = (m) => require(path.join(path.resolve(core), 'server', m));
const { createMcpWiring, createCredentialOriginCheck } = req('mcp-wiring.cjs');
const { resultToText } = req('mcp.cjs');
const { createToolRateLimit } = req('tool-rate-limit.cjs');
const internal = req('mcp-internal.cjs');

const KEY = Buffer.from('oracle-internal-key-0123456789abcdef');
const JTI = '00000000-0000-4000-8000-000000000000';
crypto.randomUUID = () => JTI;
const clock = { now: 1_800_000_000_000 };
Date.now = () => clock.now;

const CAP = 8000;
const NC_ORIGINS = ['https://cloud.example.test', 'http://nc.lan:8080'];
const OPERATOR = [
  { id: 'noevia', url: 'http://127.0.0.1:9/mcp', auth: 'internal' },
  { id: 'nextcloud', url: 'http://nc.lan:8000/mcp', auth: 'nextcloud' },
  { id: 'search', url: 'http://search.lan:9/mcp', auth: 'bearer', tokenEnv: 'SEARCH_TOKEN' },
  { id: 'nobearer', url: 'http://search2.lan:9/mcp', auth: 'bearer', tokenEnv: 'UNSET_TOKEN' },
  { id: 'open', url: 'http://open.lan:9/mcp', auth: 'none' },
];
const DIRECTORY = [
  { id: 'dir-keys', url: 'https://keys.example.test/mcp', auth: 'personal', directory: true, title: 'QA Keys', addedBy: 'admin-1' },
  { id: 'dir-untitled', url: 'https://untitled.example.test/mcp', auth: 'personal', directory: true, title: '', addedBy: 'admin-1' },
  { id: 'dir-oauth', url: 'https://oauth.example.test/mcp', auth: 'oauth', directory: true, title: 'QA Sign-in', addedBy: 'admin-1' },
  { id: 'dir-shared', url: 'https://shared.example.test/mcp', auth: 'directory', directory: true, title: 'QA Shared', addedBy: 'admin-1' },
  { id: 'dir-private', url: 'https://private.example.test/mcp', auth: 'none', directory: true, title: 'QA Moved', addedBy: 'admin-1' },
];
const SHARED_HEADERS = { 'dir-shared': { 'X-Api-Key': 'qa-shared-key' } };
const NOT_PUBLIC = new Set(['https://private.example.test/mcp']);
const WRITE_TOOLS = new Set(['project_edit_file', 'diary_append', 'nc_notes_create_note']);
const ENV = { SEARCH_TOKEN: 'qa-search-token' };
const STORAGE = {
  'user-nc': { kind: 'nextcloud', baseUrl: 'https://cloud.example.test/', username: 'qa-user', secret: 'qa-app-pass' },
  'user-dav': { kind: 'webdav', baseUrl: 'http://NC.lan:8080/remote.php/dav', username: 'qa-dav', secret: 'qa-dav-pass' },
  'user-far': { kind: 'nextcloud', baseUrl: 'https://elsewhere.example.test', username: 'qa-far', secret: 'qa-far-pass' },
  'user-nouser': { kind: 'nextcloud', baseUrl: 'https://cloud.example.test', username: '', secret: 'qa-pass' },
  'user-local': { kind: 'local', baseUrl: '', username: '', secret: '' },
};
const TOOLS = { noevia: ['project_read_file', 'project_edit_file', 'diary_append'], nextcloud: ['nc_notes_search_notes', 'nc_notes_create_note'],
  search: ['tavily_search'], nobearer: ['other_search'], open: ['open_tool', 'shared_name'], 'dir-keys': ['keys_tool'], 'dir-untitled': ['untitled_tool'],
  'dir-oauth': ['oauth_tool'], 'dir-shared': ['shared_tool', 'shared_name'], 'dir-private': ['moved_tool'] };

const r = (text, extra = {}) => ({ result: { content: [{ type: 'text', text }], ...extra } });
const http = (status, retryAfter = null) => ({ http: status, retryAfter });
const LONG = 'x'.repeat(290) + '😀😀😀😀😀😀😀😀';

/** A call: { server, name, args?, user?, workspace?, project?, editTarget?, advance?, transport: [outcome per round], keys?: [answer], sessions?: [answer] }.
 *  An outcome: { result } | { http, retryAfter? } | { error: message } | { connect: { http } } (failed at initialize).
 *  A broker answer: an object (headers, or { accessToken, generation }), null (sign in / no key) or 'transient'. */
const SCENARIOS = [
  { name: 'an operator server with no credential answers text', calls: [
    { server: 'open', name: 'open_tool', args: { q: 'qa é 😀' }, user: 'user-1', transport: [r('qa answer')] },
    { server: 'open', name: 'open_tool', transport: [{ result: { content: [{ type: 'text', text: '  a  ' }, { type: 'image', data: 'x' }, { type: 'text', text: 'b' }, { type: { a: 1 } }, { type: 'text', text: 5 }, 'str', 7, null] } }] },
    { server: 'open', name: 'open_tool', transport: [{ result: { content: [], structuredContent: { rows: [1, 'two', { three: null }] } } }] },
    { server: 'open', name: 'open_tool', transport: [{ result: { content: [{ type: 'text', text: '   ' }], structuredContent: 'just text' } }] },
    { server: 'open', name: 'open_tool', transport: [{ result: { content: [] } }] },
    { server: 'open', name: 'open_tool', transport: [{ result: undefined }] },
    { server: 'open', name: 'open_tool', transport: [{ result: null }] },
    { server: 'open', name: 'open_tool', transport: [{ result: { content: 'a string' } }] },
    { server: 'open', name: 'open_tool', transport: [{ result: { content: 5 } }] },
    { server: 'open', name: 'open_tool', transport: [{ result: { content: 1.5 } }] },
    { server: 'open', name: 'open_tool', transport: [{ result: { content: { a: 1 } } }] },
    { server: 'open', name: 'open_tool', transport: [{ result: { content: true } }] },
    { server: 'open', name: 'open_tool', transport: [{ result: { content: false, structuredContent: 0 } }] },
    { server: 'open', name: 'open_tool', transport: [{ result: { isError: true, content: [] } }] },
    { server: 'open', name: 'open_tool', transport: [{ result: { isError: 1, content: [{ type: 'text', text: 'bad qa input' }] } }] },
    { server: 'open', name: 'open_tool', transport: [r('x'.repeat(9000))] },
  ] },
  { name: 'failures are text the model can act on', calls: [
    { server: 'open', name: 'open_tool', transport: [http(500)] },
    { server: 'open', name: 'open_tool', transport: [http(404)] },
    { server: 'open', name: 'open_tool', transport: [{ connect: { http: 502 } }] },
    { server: 'open', name: 'open_tool', transport: [{ error: 'fetch failed' }] },
    { server: 'open', name: 'open_tool', transport: [{ error: 'MCP error -32602: bad qa args' }] },
    { server: 'open', name: 'open_tool', transport: [{ error: LONG }] },
    { server: 'open', name: 'open_tool', transport: [{ error: 'This operation was aborted' }] },
  ] },
  { name: 'a 429 backs the provider off, a success clears it', calls: [
    { server: 'search', name: 'tavily_search', args: { query: 'qa' }, user: 'user-1', transport: [http(429, '7')] },
    { server: 'search', name: 'tavily_search', user: 'user-2', advance: 3000 },
    { server: 'search', name: 'tavily_search', user: 'user-1', advance: 4500, transport: [http(429)] },
    { server: 'search', name: 'tavily_search', user: 'user-1', advance: 31_000, transport: [{ result: { isError: true, content: [{ type: 'text', text: 'Too Many Requests: quota exceeded' }] } }] },
    { server: 'search', name: 'tavily_search', user: 'user-1', advance: 60_001, transport: [r('qa results')] },
    { server: 'search', name: 'tavily_search', user: 'user-1', transport: [http(429, 'Fri, 15 Jan 2027 08:00:10 GMT')] },
    { server: 'search', name: 'tavily_search', user: 'user-1', advance: 400_000, transport: [http(429, 'soon')] },
    { server: 'search', name: 'tavily_search', user: 'user-1', advance: 400_000, transport: [{ result: { isError: true, content: [{ type: 'text', text: 'rate limited' }] } }] },
    { server: 'search', name: 'tavily_search', user: 'user-1', advance: 400_000, transport: [{ result: { isError: true, content: [{ type: 'text', text: 'not found' }] } }] },
    { server: 'search', name: 'tavily_search', user: 'user-1', transport: [http(429, '0')] },
  ] },
  { name: 'a bearer server sends its service token or says what is missing', calls: [
    { server: 'search', name: 'tavily_search', transport: [r('ok')] },
    { server: 'nobearer', name: 'other_search', transport: [] },
  ] },
  { name: 'a personal key is the caller own, asked for this call only', calls: [
    { server: 'dir-keys', name: 'keys_tool', user: 'user-1', keys: [{ 'X-Api-Key': 'qa-key-1', Accept: 'text/html', 'mcp-session-id': 'hijack' }], transport: [r('keyed')] },
    { server: 'dir-keys', name: 'keys_tool', user: 'user-1', keys: [null], transport: [] },
    { server: 'dir-keys', name: 'keys_tool', user: 'user-1', keys: [{}], transport: [] },
    { server: 'dir-keys', name: 'keys_tool', user: 'user-1', keys: ['transient'], transport: [] },
    { server: 'dir-untitled', name: 'untitled_tool', user: 'user-1', keys: ['transient'], transport: [] },
    { server: 'dir-untitled', name: 'untitled_tool', user: 'user-1', keys: [null], transport: [] },
    { server: 'dir-keys', name: 'keys_tool', transport: [] },
    { server: 'dir-keys', name: 'keys_tool', user: 'user-1', keys: [{ 'X-Api-Key': 'qa-key-1' }], transport: [http(429)] },
    { server: 'dir-keys', name: 'keys_tool', user: 'user-2', keys: [{ 'X-Api-Key': 'qa-key-2' }], transport: [r('other account not paused')] },
    { server: 'dir-keys', name: 'keys_tool', user: 'user-1', keys: [{ 'X-Api-Key': 'qa-key-1' }], transport: [] },
  ] },
  { name: 'a sign-in is refreshed past a refused token once', calls: [
    { server: 'dir-oauth', name: 'oauth_tool', user: 'user-1', sessions: [{ accessToken: 'qa-at-1', generation: 'g1' }], transport: [r('signed in')] },
    { server: 'dir-oauth', name: 'oauth_tool', user: 'user-1', sessions: [null], transport: [] },
    { server: 'dir-oauth', name: 'oauth_tool', user: 'user-1', sessions: ['transient'], transport: [] },
    { server: 'dir-oauth', name: 'oauth_tool', transport: [] },
    { server: 'dir-oauth', name: 'oauth_tool', user: 'user-1', sessions: [{ accessToken: 'qa-at-1', generation: 'g1' }, { accessToken: 'qa-at-2', generation: 'g2' }], transport: [http(401), r('after refresh')] },
    { server: 'dir-oauth', name: 'oauth_tool', user: 'user-1', sessions: [{ accessToken: 'qa-at-1', generation: 'g1' }, { accessToken: 'qa-at-1', generation: 'g1' }], transport: [http(401)] },
    { server: 'dir-oauth', name: 'oauth_tool', user: 'user-1', sessions: [{ accessToken: 'qa-at-1', generation: 'g1' }, null], transport: [http(401)] },
    { server: 'dir-oauth', name: 'oauth_tool', user: 'user-1', sessions: [{ accessToken: 'qa-at-1', generation: 'g1' }, 'transient'], transport: [http(401)] },
    { server: 'dir-oauth', name: 'oauth_tool', user: 'user-1', sessions: [{ accessToken: 'qa-at-1', generation: 'g1' }, { accessToken: 'qa-at-2', generation: 'g2' }], transport: [http(401), http(401)] },
    { server: 'dir-oauth', name: 'oauth_tool', user: 'user-1', sessions: [{ accessToken: 'qa-at-1', generation: 'g1' }, { accessToken: 'qa-at-2', generation: 'g2' }], transport: [{ connect: { http: 401 } }, http(500)] },
    { server: 'dir-oauth', name: 'oauth_tool', user: 'user-1', sessions: [{ accessToken: 'qa-at-1', generation: 'g1' }, { accessToken: 'qa-at-2', generation: 'g2' }], transport: [http(401), http(429, '12')] },
    { server: 'dir-oauth', name: 'oauth_tool', user: 'user-1', advance: 20_000, sessions: [{ accessToken: 'qa-at-1', generation: 'g1' }], transport: [http(403)] },
  ] },
  { name: 'directory servers: shared headers, and the address is checked again', calls: [
    { server: 'dir-shared', name: 'shared_tool', user: 'user-1', transport: [r('shared ok')] },
    { server: 'dir-private', name: 'moved_tool', user: 'user-1', transport: [] },
    { server: 'dir-shared', name: 'shared_name', user: 'user-1', transport: [r('the directory server')] },
    { server: 'open', name: 'shared_name', user: 'user-1', transport: [r('the operator server')] },
  ] },
  { name: 'the in-process server gets a capability token for this user and project', calls: [
    { server: 'noevia', name: 'project_read_file', user: 'user-1', workspace: 'user-1', project: 'proj-1', transport: [r('file text')] },
    { server: 'noevia', name: 'project_edit_file', user: 'user-1', workspace: 'user-1', project: 'proj-1', editTarget: 'digest-1', transport: [r('edited')] },
    { server: 'noevia', name: 'diary_append', user: 'admin-1', workspace: 'user-3', transport: [r('appended')] },
    { server: 'noevia', name: 'project_read_file', user: 'user-1', workspace: 'user-1', project: 'proj-1', editTarget: 'digest-2', transport: [r('read with a target')] },
    { server: 'noevia', name: 'project_read_file', user: 'user-1', transport: [] },
    { server: 'noevia', name: 'project_edit_file', user: 'user-1', workspace: 'user-1', editTarget: '', transport: [r('edit, empty target')] },
  ] },
  { name: 'an approval for a project file edit is never sent elsewhere', calls: [
    { server: 'open', name: 'open_tool', user: 'user-1', workspace: 'user-1', project: 'proj-1', editTarget: 'digest-1', transport: [] },
    { server: 'dir-keys', name: 'keys_tool', user: 'user-1', editTarget: 'digest-1', transport: [] },
  ] },
  { name: 'nextcloud pass-through only to a declared origin', calls: [
    { server: 'nextcloud', name: 'nc_notes_search_notes', user: 'user-nc', workspace: 'user-nc', transport: [r('notes')] },
    { server: 'nextcloud', name: 'nc_notes_create_note', user: 'user-dav', workspace: 'user-dav', transport: [r('created')] },
    { server: 'nextcloud', name: 'nc_notes_search_notes', user: 'user-far', workspace: 'user-far', transport: [] },
    { server: 'nextcloud', name: 'nc_notes_search_notes', user: 'user-nouser', workspace: 'user-nouser', transport: [] },
    { server: 'nextcloud', name: 'nc_notes_search_notes', user: 'user-local', workspace: 'user-local', transport: [] },
    { server: 'nextcloud', name: 'nc_notes_search_notes', user: 'user-nc', transport: [] },
    { server: 'nextcloud', name: 'nc_notes_search_notes', user: 'user-nc', workspace: 'user-nc', transport: [http(429)] },
    { server: 'nextcloud', name: 'nc_notes_search_notes', user: 'user-dav', workspace: 'user-dav', transport: [r('another account is not paused')] },
  ] },
];

function snapshot() {
  const servers = [...OPERATOR, ...DIRECTORY].map((s) => ({ id: s.id, url: s.url, auth: s.auth, error: null, discoveredAt: 1, toolCount: TOOLS[s.id].length }));
  const offers = [];
  for (const s of [...OPERATOR, ...DIRECTORY]) for (const name of TOOLS[s.id]) {
    offers.push({ name, server: s.id, readOnly: !WRITE_TOOLS.has(name), tool: { type: 'function', function: { name, description: name, parameters: { type: 'object', properties: {}, required: [] } } } });
  }
  return { version: 1, incarnation: 'oracle', generation: 1, enabled: true, error: null, discoveredAt: 1, failedAt: 0, remoteFailStreak: 0, servers, offers, boxes: [] };
}

async function run(scenario) {
  const scope = new AsyncLocalStorage();
  let rounds = [];
  let broker = [];
  let script = null;
  const answer = (queue, what) => {
    if (!queue.length) throw new Error(`oracle: no scripted ${what} in ${scenario.name}`);
    const a = queue.shift();
    if (a === 'transient') throw Object.assign(new Error('Your sign-in or key could not be checked right now. Try again in a moment.'), { status: 503, code: 'MCP_BROKER_UNAVAILABLE', transient: true });
    return a === null ? null : JSON.parse(JSON.stringify(a));
  };
  const mcp = {
    resultToText,
    async connect(url, headers) {
      const outcome = script.transport[0];
      rounds.push({ url, headers: { ...(headers || {}) } });
      if (outcome && outcome.connect) { script.transport.shift(); throw Object.assign(new Error(`MCP ${outcome.connect.http}: qa`), { httpStatus: outcome.connect.http, retryAfter: null }); }
      return { session: { id: 'qa-session' } };
    },
    async callTool(url, session, name, args) {
      const outcome = script.transport.shift();
      if (!outcome) throw new Error(`oracle: no scripted transport outcome in ${scenario.name}`);
      const round = rounds[rounds.length - 1];
      round.name = name; round.args = JSON.parse(JSON.stringify(args ?? null));
      if (outcome.http) throw Object.assign(new Error(`MCP ${outcome.http}: qa detail`), { httpStatus: outcome.http, retryAfter: outcome.retryAfter ?? null });
      if (outcome.error) throw new Error(outcome.error);
      return outcome.result === undefined ? undefined : JSON.parse(JSON.stringify(outcome.result));
    },
    async disconnect() { return true; },
  };
  const wiring = createMcpWiring({
    servers: OPERATOR.map((s) => ({ ...s })),
    mcp,
    discovery: { discover: async () => snapshot() },
    directoryMcp: { asServers: () => DIRECTORY.map((s) => ({ ...s })), boxFor: () => null, headersFor: (id) => SHARED_HEADERS[id] || {}, signal: () => 'static' },
    // As rust-mcp-credentials.cjs: no account is no credential, the broker is not asked.
    mcpOAuth: {
      connected: () => true,
      tokenFor: async () => { throw new Error('oracle: tokenFor is not used'); },
      async session(user, server, rejected) { if (!user) return null; broker.push({ op: 'session', user, server, rejected: rejected ?? null }); return answer(script.sessions || [], 'session'); },
      async reject(user, server, generation) { broker.push({ op: 'reject', user, server, generation }); return true; },
    },
    mcpKeys: {
      hasKey: () => true,
      async headers(user, server) { if (!user) return null; broker.push({ op: 'key', user, server }); return answer(script.keys || [], 'key'); },
    },
    directoryUrlAllowed: async (url) => !NOT_PUBLIC.has(url),
    credentialOriginAllowed: createCredentialOriginCheck(NC_ORIGINS),
    scope,
    storageFor: (userId) => STORAGE[userId] || { kind: 'local', baseUrl: '', username: '' },
    isWriteTool: (name) => WRITE_TOOLS.has(name),
    internal,
    internalKey: KEY,
    reduceToolResult: (text, { maxChars }) => ({ text: `[reduced to ${maxChars}] ${text}`, reduced: false }),
    resultCap: CAP,
    env: ENV,
    logger: { log() {}, warn() {} },
    toolRateLimit: createToolRateLimit(),
    directoryCheckMs: 0,
  });
  await wiring.discoverMcpTools(true);
  const calls = [];
  for (const call of scenario.calls) {
    if (call.advance) clock.now += call.advance;
    script = { transport: [...(call.transport || [])], keys: [...(call.keys || [])], sessions: [...(call.sessions || [])] };
    rounds = []; broker = [];
    const store = {
      ...(call.workspace ? { workspace: { userId: call.workspace } } : {}),
      ...(call.user ? { authn: { user: { id: call.user } } } : {}),
      internalCallProject: call.project ? { id: call.project } : null,
      internalEditTarget: typeof call.editTarget === 'string' ? call.editTarget : null,
    };
    const args = call.args || { qa: true };
    const nextcloud = scope.run(store, () => wiring.mcpAuthHeaders());
    const text = await scope.run(store, () => wiring.executeMcpToolCall(call.name, args, undefined, call.server));
    if (script.transport.length || script.keys.length || script.sessions.length) throw new Error(`oracle: unused script in ${scenario.name} / ${call.name}`);
    calls.push({
      at: clock.now,
      request: { server: call.server, name: call.name, arguments: args, user: call.user || null, workspaceUser: call.workspace || null,
        project: call.project || null, editTarget: typeof call.editTarget === 'string' ? call.editTarget : null, write: WRITE_TOOLS.has(call.name), nextcloud: nextcloud || null },
      script: { transport: call.transport || [], keys: call.keys || [], sessions: call.sessions || [] },
      rounds, broker, text,
    });
  }
  return { name: scenario.name, calls };
}

(async () => {
  const scenarios = [];
  for (const s of SCENARIOS) scenarios.push(await run(s));
  const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(path.resolve(core), 'server', f))).digest('hex');
  const fixture = {
    version: 1,
    generator: 'noevia-rs bins/noevia-server/tests/mcp-call-oracle.cjs (from noevia-core server/mcp-wiring.cjs executeMcpToolCall)',
    sources: Object.fromEntries(SOURCES.map((f) => [`server/${f}`, sha(f)])),
    coreRef: process.env.NOEVIA_CORE_REF || null,
    internalKey: KEY.toString('hex'), jti: JTI, resultCap: CAP, reduceMarker: '[reduced to 8000] ',
    servers: [...OPERATOR, ...DIRECTORY], sharedHeaders: SHARED_HEADERS, notPublic: [...NOT_PUBLIC], env: ENV,
    scenarios,
  };
  fs.writeFileSync(out, JSON.stringify(fixture, null, 1) + '\n');
})().catch((err) => { process.stderr.write(`${err.stack || err}\n`); process.exit(1); });
