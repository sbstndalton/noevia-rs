'use strict';
// Test-only Node oracle for the native MCP discovery actor (sbstndalton/noevia row T). For every scenario
// it runs core's REAL discovery wiring (server/mcp-wiring.cjs createMcpWiring, mcp-boxes.cjs bindBoxes,
// directory-mcp.cjs, toolboxes.cjs, mcp-toolbox-manifest.cjs, mcp-internal.cjs mintToken) at the checkout
// given - CI: the ref in tests/fixtures/mcp-discovery-oracle.core-ref, the last core whose wiring owns
// discovery - with a scripted MCP transport and a virtual clock, and records after every step what the
// process knows (the tools by name and by server, the boxes, each server's last discovery, the
// timestamps and the backoff streak) and which servers were listed with what credential. Synthetic data
// only; nothing is contacted.
//
//   node bins/noevia-server/tests/mcp-discovery-oracle.cjs <core-checkout> <out.json>
//
// The output is deterministic. CI regenerates it and compares it with the committed
// tests/fixtures/mcp-discovery-node.v1.json, which tests/mcp_discovery_actor.rs replays through the actor.
process.env.TZ = 'UTC';
console.warn = () => {};
console.log = () => {};
const fs = require('node:fs');
const crypto = require('node:crypto');
const path = require('node:path');

const [core, out] = process.argv.slice(2);
if (!core || !out) throw new Error('Usage: node mcp-discovery-oracle.cjs <core-checkout> <out.json>');
const req = (m) => require(path.join(path.resolve(core), 'server', m));
const { createMcpWiring } = req('mcp-wiring.cjs');
const { bindBoxes } = req('mcp-boxes.cjs');
const { createDirectoryMcp } = req('directory-mcp.cjs');
const { buildToolboxManifest } = req('mcp-toolbox-manifest.cjs');
const { createToolboxes } = req('toolboxes.cjs');
const { buildCatalogue } = req('mcp-discovery-snapshot.cjs');
const internal = req('mcp-internal.cjs');
const { createToolboxPickerFacts } = req('rust-toolbox-picker-facts.cjs');
const { resolveSchemaRefsJs } = require(path.join(path.resolve(core), 'tests', 'server', 'oracle', 'mcp.cjs'));

// core mcp.cjs convertTool and readOnlyHint, with the JS twin of the schema resolver (the wasm is not
// needed to record how the wiring uses them; the Rust side has its own differential for the resolver).
const TOOL_NAME_RE = /^[A-Za-z0-9_-]{1,64}$/;
function convertTool(mcpTool) {
  const name = mcpTool && mcpTool.name;
  if (typeof name !== 'string' || !TOOL_NAME_RE.test(name)) return { ok: false, reason: `invalid tool name ${JSON.stringify(name)}` };
  const schema = mcpTool.inputSchema;
  if (!schema || typeof schema !== 'object' || Array.isArray(schema)) return { ok: false, reason: 'inputSchema missing or not an object' };
  if (schema.type !== 'object') return { ok: false, reason: `inputSchema.type is ${JSON.stringify(schema.type)}, expected "object"` };
  if (schema.properties !== undefined && (typeof schema.properties !== 'object' || Array.isArray(schema.properties))) return { ok: false, reason: 'inputSchema.properties is not an object' };
  if (schema.required !== undefined && !Array.isArray(schema.required)) return { ok: false, reason: 'inputSchema.required is not an array' };
  let resolved;
  try { resolved = resolveSchemaRefsJs(schema); } catch (err) { return { ok: false, reason: `unresolvable schema: ${err.message}` }; }
  const description = typeof mcpTool.description === 'string' && mcpTool.description ? mcpTool.description : (typeof mcpTool.title === 'string' ? mcpTool.title : '');
  return { ok: true, tool: { type: 'function', function: { name, description, parameters: { type: 'object', properties: resolved.properties || {}, required: resolved.required || [] } } } };
}
function readOnlyHint(mcpTool) {
  const a = mcpTool && mcpTool.annotations;
  if (!a || typeof a !== 'object') return null;
  return typeof a.readOnlyHint === 'boolean' ? a.readOnlyHint : null;
}

const KEY = Buffer.from('oracle-internal-key');
const clock = { now: 1_000_000 };
Date.now = () => clock.now;
const tool = (name, extra = {}) => ({ name, description: `${name} does a thing`, inputSchema: { type: 'object', properties: { q: { type: 'string' } }, required: [] }, ...extra });
const ro = (name, readOnlyHint = true) => tool(name, { annotations: { readOnlyHint } });

const NEXTCLOUD_TOOLS = [ro('nc_notes_search_notes'), ro('nc_notes_get_note'), tool('nc_notes_create_note'), ro('nc_calendar_list_todos', true), tool('nc_calendar_create_todo'), ro('nc_news_list_feeds', false)];
const SEARCH_TOOLS = [ro('tavily_search'), ro('tavily_extract'), tool('tavily_crawl'), tool('bad name', {}), { name: 'no_schema' }, { name: 'array_schema', inputSchema: { type: 'array' } }];
const INTERNAL_TOOLS = [ro('diary_read_today'), ro('diary_read_month'), ro('diary_list_months'), tool('diary_append'), ro('project_list_files'), ro('project_read_file'), ro('project_search'), tool('project_edit_file')];

const BASE_SERVERS = [
  { id: 'noevia', url: 'http://127.0.0.1:9/mcp', auth: 'internal' },
  { id: 'nextcloud', url: 'http://nc.lan:8000/mcp', auth: 'nextcloud' },
  { id: 'search', url: 'http://search.lan:9/mcp', auth: 'bearer', tokenEnv: 'SEARCH_TOKEN' },
];
const BASE_TOOLS = { noevia: INTERNAL_TOOLS, nextcloud: NEXTCLOUD_TOOLS, search: SEARCH_TOOLS };
const dirRow = (id, over = {}) => ({ id, registryName: id, title: `Title ${id}`, url: `https://${id}.example.test/mcp`, addedBy: 'admin-1', addedAt: 1, headersEnc: null, oauth: 0, personal: 0, declaredJson: '[]', ...over });

/** Scenarios: `servers` (operator list), `tools` (server id -> listed tools), `dir` (directory rows), `steps`.
 *  A step: { at?: ms to set the clock to, set?: { tools: {id: [..]|null}, fail: {id: message|null}, dir: rows, ready: {...}, disabled: [..], public: {...}, token: {...} },
 *  op: 'discover' | 'force' | 'parallel', ops: [...] for 'parallel' (each 'discover' or 'force'), gate: true to hold the listings until released }. */
const SCENARIOS = [
  { name: 'first discovery binds the curated boxes', servers: BASE_SERVERS, tools: BASE_TOOLS, steps: [{ op: 'discover' }, { op: 'discover', at: 1_000_500 }] },
  { name: 'a fresh discovery is reused until its ttl, then listed again', servers: BASE_SERVERS, tools: BASE_TOOLS, steps: [{ op: 'discover' }, { at: 1_000_000 + 599_000, op: 'discover' }, { at: 1_000_000 + 601_000, op: 'discover' }, { at: 1_000_000 + 601_100, op: 'force' }] },
  { name: 'diaryMcpWrite adds diary_append to the diary box', diaryMcpWrite: true, servers: BASE_SERVERS, tools: BASE_TOOLS, steps: [{ op: 'discover' }] },
  { name: 'a name offered by two servers keeps the first and routes by offers', servers: [...BASE_SERVERS, { id: 'search2', url: 'http://search2.lan:9/mcp', auth: 'none' }], tools: { ...BASE_TOOLS, search2: [ro('tavily_search'), tool('only_here')] }, steps: [{ op: 'discover' }] },
  { name: 'a remote server that fails is retried on the backoff steps and recovers', servers: BASE_SERVERS, tools: BASE_TOOLS, fail: { nextcloud: 'fetch failed' }, steps: [
    { op: 'discover' }, { at: 1_004_000, op: 'discover' }, { at: 1_005_100, op: 'discover' }, { at: 1_019_000, op: 'discover' }, { at: 1_020_500, op: 'discover' }, { at: 1_064_000, op: 'discover' }, { at: 1_066_000, op: 'discover' },
    { at: 1_180_000, op: 'discover' }, { at: 1_187_000, op: 'discover' }, { at: 1_310_000, op: 'discover' }, { at: 1_320_000, set: { fail: { nextcloud: null } }, op: 'force' }, { at: 1_330_000, op: 'discover' }] },
  { name: 'the internal server being down is retried on the very next request', servers: BASE_SERVERS, tools: BASE_TOOLS, fail: { noevia: 'fetch failed' }, steps: [{ op: 'discover' }, { at: 1_000_010, op: 'discover' }, { at: 1_000_020, set: { fail: { noevia: null } }, op: 'discover' }, { at: 1_000_030, op: 'discover' }] },
  { name: 'every server failing is remembered for the short failure ttl', servers: [BASE_SERVERS[1]], tools: { nextcloud: NEXTCLOUD_TOOLS }, fail: { nextcloud: 'MCP 502: bad gateway' }, steps: [{ op: 'discover' }, { at: 1_020_000, op: 'discover' }, { at: 1_031_000, op: 'discover' }, { at: 1_031_100, op: 'force' }] },
  { name: 'no servers configured offers nothing', servers: [], tools: {}, steps: [{ op: 'discover' }, { op: 'force' }] },
  { name: 'a bearer server without its token variable fails with the auth left out', servers: [BASE_SERVERS[2]], tools: { search: SEARCH_TOOLS }, noToken: true, steps: [{ op: 'discover' }] },
  { name: 'directory servers: credentials, boxes and per-account states', servers: [BASE_SERVERS[0]], tools: { noevia: INTERNAL_TOOLS, 'dir-open': [tool('dir_a'), tool('dir_b')], 'dir-key': [tool('key_a')], 'dir-oauth': [tool('oauth_a')], 'dir-personal': [tool('personal_a')] },
    dir: [dirRow('dir-open'), dirRow('dir-key', { headersEnc: 'enc-key' }), dirRow('dir-oauth', { oauth: 1 }), dirRow('dir-personal', { personal: 1, addedBy: 'admin-2' })],
    ready: { oauth: { 'admin-1:dir-oauth': 'oauth-token-1' }, key: { 'admin-2:dir-personal': { 'X-Api-Key': 'personal-key-2' } } },
    steps: [{ op: 'discover' }] },
  { name: 'directory servers waiting for their administrator, or one that is disabled', servers: [], tools: { 'dir-oauth': [tool('oauth_a')], 'dir-personal': [tool('personal_a')] },
    dir: [dirRow('dir-oauth', { oauth: 1 }), dirRow('dir-personal', { personal: 1, addedBy: 'admin-2' })], disabled: ['admin-2'],
    steps: [{ op: 'discover' }, { at: 1_040_000, set: { ready: { oauth: { 'admin-1:dir-oauth': 'oauth-token-1' }, key: { 'admin-2:dir-personal': { 'X-Api-Key': 'k' } } } }, op: 'discover' }, { at: 1_041_000, set: { disabled: [] }, op: 'discover' }] },
  { name: 'a directory server whose address stops being public is refused at discovery', servers: [], tools: { 'dir-open': [tool('dir_a')] }, dir: [dirRow('dir-open')], steps: [{ op: 'discover' }, { at: 1_001_000, set: { public: { 'dir-open': false } }, op: 'force' }] },
  { name: 'the table changing, a server added then removed, forces a new discovery', servers: [BASE_SERVERS[1]], tools: { nextcloud: NEXTCLOUD_TOOLS, 'dir-open': [tool('dir_a')], 'dir-two': [tool('two_a')] }, dir: [],
    steps: [{ op: 'discover' }, { at: 1_001_000, set: { dir: [dirRow('dir-open')] }, op: 'discover' }, { at: 1_002_000, set: { dir: [dirRow('dir-open'), dirRow('dir-two')] }, op: 'discover' }, { at: 1_003_000, set: { dir: [] }, op: 'discover' }] },
  { name: 'a credential becoming ready is noticed without a force', servers: [], tools: { 'dir-oauth': [tool('oauth_a')] }, dir: [dirRow('dir-oauth', { oauth: 1 })],
    steps: [{ op: 'discover' }, { at: 1_005_000, set: { ready: { oauth: { 'admin-1:dir-oauth': 'oauth-token-1' } } }, op: 'discover' }, { at: 1_006_000, op: 'discover' }] },
  { name: 'unforced calls share one listing; a forced call during it waits and lists again', servers: BASE_SERVERS, tools: BASE_TOOLS, steps: [{ op: 'parallel', ops: ['discover', 'discover', 'discover'], gate: true }, { at: 1_000_100, op: 'parallel', ops: ['force', 'force'], gate: true }, { at: 1_000_200, op: 'parallel', ops: ['discover', 'force'], gate: true }] },
  { name: 'tools that cannot be converted are dropped and the rest are kept', servers: [BASE_SERVERS[2]], tools: { search: [ro('tavily_search'), { name: 'bad name!', inputSchema: { type: 'object' } }, { name: 't', inputSchema: { type: 'object', properties: [] } }, { name: 'u', inputSchema: { type: 'object', required: 'x' } }, tool('tavily_extract', { description: '', title: 'A title' })] }, steps: [{ op: 'discover' }] },
  { name: 'a curated box missing some tools keeps the ones that exist, and a box with none is not shown', servers: BASE_SERVERS, tools: { noevia: [ro('diary_read_today')], nextcloud: [], search: [ro('tavily_search')] }, steps: [{ op: 'discover' }] },
  { name: 'a hint is appended once to a curated tool and never twice', servers: [BASE_SERVERS[1]], tools: { nextcloud: [ro('nc_calendar_list_todos'), tool('nc_calendar_create_todo'), tool('nc_calendar_update_todo'), ro('nc_calendar_search_todos')] }, steps: [{ op: 'discover' }, { at: 1_700_000, op: 'discover' }] },
];

/** JSON with the keys of every object sorted: what the Rust replay hashes. */
function canon(v) {
  if (Array.isArray(v)) return `[${v.map(canon).join(',')}]`;
  if (v && typeof v === 'object') return `{${Object.keys(v).sort().map((k) => `${JSON.stringify(k)}:${canon(v[k])}`).join(',')}}`;
  return JSON.stringify(v);
}
function safeJson(v) { return JSON.parse(JSON.stringify(v === undefined ? null : v)); }

async function run(scenario) {
  clock.now = 1_000_000;
  const state = {
    tools: JSON.parse(JSON.stringify(scenario.tools || {})),
    fail: { ...(scenario.fail || {}) },
    dir: JSON.parse(JSON.stringify(scenario.dir || [])),
    ready: { oauth: {}, key: {}, ...JSON.parse(JSON.stringify(scenario.ready || {})) },
    disabled: new Set(scenario.disabled || []),
    pub: {},
  };
  const gate = { open: Promise.resolve(), release: () => {} };
  const calls = [];
  const logs = [];
  const logger = { log: (m) => logs.push(`log ${m}`), warn: (m) => logs.push(`warn ${m}`) };
  const headerSummary = (headers) => {
    if (!headers) return null;
    const rows = Object.entries(headers).map(([k, v]) => {
      if (k === 'Authorization' && /^Bearer [^.]+\.[^.]+$/.test(v)) {
        const checked = internal.verifyToken(KEY, v.slice(7), clock.now);
        if (checked.ok) {
          const c = checked.claims;
          return [k, `internal-token uid=${c.uid} pid=${c.pid} w=${c.w} d=${c.d} ttl=${c.exp - clock.now}`];
        }
      }
      return [k, v];
    });
    return rows.sort((a, b) => (a[0] < b[0] ? -1 : 1));
  };
  const mcp = {
    convertTool, readOnlyHint,
    async connect(url, headers) { return { session: { url, headers } }; },
    async listTools(url, session, headers) {
      const server = [...BASE_SERVERS, ...state.dir.map((r) => ({ id: r.id, url: r.url }))].concat(scenario.servers || []).find((s) => s.url === url);
      const id = server ? server.id : url;
      calls.push({ server: id, headers: headerSummary(headers) });
      await gate.open;
      if (state.fail[id]) throw new Error(state.fail[id]);
      if (!Object.hasOwn(state.tools, id) || state.tools[id] === null) throw new Error('fetch failed');
      return JSON.parse(JSON.stringify(state.tools[id]));
    },
    async disconnect() {},
  };
  const fakeDb = {
    exec() {},
    prepare(sql) {
      if (/pragma_table_info/.test(sql)) return { get: () => ({ 1: 1 }) };
      return { all: () => state.dir.map((r) => ({ ...r })) };
    },
  };
  const directoryMcp = createDirectoryMcp({ db: fakeDb, secrets: { decrypt: (enc) => JSON.stringify(enc === 'enc-key' ? { 'X-Shared-Key': 'shared-1' } : {}) } });
  const servers = JSON.parse(JSON.stringify(scenario.servers || []));
  const env = scenario.noToken ? {} : { SEARCH_TOKEN: 'search-token-1' };
  const wiring = createMcpWiring({
    servers, manifest: buildToolboxManifest({ diaryMcpWrite: !!scenario.diaryMcpWrite }), mcp, bindBoxes, directoryMcp,
    mcpOAuth: { connected: (u, s) => !!state.ready.oauth[`${u}:${s}`], tokenFor: async (u, s) => state.ready.oauth[`${u}:${s}`] || null },
    mcpKeys: { hasKey: (u, s) => !!state.ready.key[`${u}:${s}`], headers: async (u, s) => state.ready.key[`${u}:${s}`] || null },
    directoryUrlAllowed: async (url) => { const r = state.dir.find((x) => x.url === url); return !(r && state.pub[r.id] === false); },
    credentialOriginAllowed: () => false, scope: { getStore: () => undefined }, storageFor: () => ({}),
    isWriteTool: () => true, internal, internalKey: KEY, reduceToolResult: (t) => ({ text: t, reduced: false }),
    env, logger, isUserDisabled: (id) => state.disabled.has(id), directoryCheckMs: 0,
  });
  const manifest = buildToolboxManifest({ diaryMcpWrite: !!scenario.diaryMcpWrite });
  const mcpState = wiring.state;
  const toolboxes = createToolboxes({ boxes: [], mcpBoxes: () => mcpState.boxes, mcpTools: () => mcpState.tools, offered: () => true, prefill: { budgetFor: () => null, rateFor: () => 0, stats: () => [] }, documentSources: {} });
  const summary = (withCatalogue) => safeJson({
    enabled: wiring.enabled(),
    error: mcpState.error,
    discoveredAt: mcpState.discoveredAt,
    failedAt: mcpState.failedAt,
    remoteFailStreak: mcpState.remoteFailStreak,
    servers: [...mcpState.servers.values()].map((s) => ({ id: s.id, url: s.url, auth: s.auth, directory: !!s.directory, error: s.error, discoveredAt: s.discoveredAt, toolCount: s.toolCount })),
    tools: [...mcpState.tools].map(([name, e]) => ({ name, serverId: e.serverId, readOnly: e.readOnly, toolSha256: crypto.createHash('sha256').update(canon(e.tool)).digest('hex') })),
    offers: [...mcpState.offers].map(([name, m]) => ({ name, servers: [...m.keys()] })),
    boxes: mcpState.boxes,
    catalogue: !withCatalogue ? null : buildCatalogue({
      allToolboxes: () => toolboxes.allToolboxes(), manifest: manifest.filter(() => true), mcpEnabled: () => wiring.enabled(), servers, state: mcpState,
      isWriteTool: (name) => toolboxes.isWriteTool(name), harnessEnabled: () => false, hasRepositories: () => false, nextcloudOrigins: [], defaultToolboxes: toolboxes.DEFAULT_TOOLBOXES, connectorBoxes: toolboxes.CONNECTOR_BOXES,
    }),
  });
  // What the chat process would publish of itself (the Rust front composes the catalogue and the picker from it and its own
  // discovery): the built-in boxes (no inApp key when core's has none), the connector boxes, the defaults, the prefill.
  const prefillStats = { m1: { rate: 0.2, samples: 3 } };
  const factsDoc = safeJson({
    builtin: toolboxes.TOOLBOXES.map((b) => ({ id: b.id, label: b.label, description: b.description, source: b.source, ...(b.inApp === undefined ? {} : { inApp: b.inApp }), reads: [...(b.reads || [])], tools: b.tools })),
    defaults: toolboxes.DEFAULT_TOOLBOXES, connectorBoxes: [...toolboxes.CONNECTOR_BOXES], harnessEnabled: false, hasRepositories: false,
    prefill: { targetMs: 1250, models: prefillStats },
  });
  /** What the old picker bridge answered, with the discovery it asked for already done. */
  async function pickerFacts() {
    let body = null;
    const handler = createToolboxPickerFacts({
      secret: 's', discoverMcpTools: async () => {}, allToolboxes: () => toolboxes.allToolboxes(), connectorBoxes: toolboxes.CONNECTOR_BOXES,
      prefill: { targetMs: 1250, stats: () => prefillStats }, mcp: () => ({ enabled: wiring.enabled(), state: mcpState, servers, manifest }),
      json: (_res, status, doc) => { body = { status, doc }; }, readJson: async () => ({}), log: () => {},
    });
    await handler({ url: '/internal/toolbox-picker/facts', method: 'POST', socket: { remoteAddress: '127.0.0.1' }, headers: { 'x-noevia-store-token': 's' } }, {});
    return safeJson(body);
  }
  const steps = [];
  let previous = null;
  for (const step of scenario.steps) {
    if (step.at !== undefined) clock.now = step.at;
    const set = step.set || {};
    if (set.tools) for (const [k, v] of Object.entries(set.tools)) state.tools[k] = v;
    if (set.fail) for (const [k, v] of Object.entries(set.fail)) { if (v) state.fail[k] = v; else delete state.fail[k]; }
    if (set.dir) state.dir = JSON.parse(JSON.stringify(set.dir));
    if (set.ready) state.ready = { oauth: { ...(set.ready.oauth || {}) }, key: { ...(set.ready.key || {}) } };
    if (set.disabled) state.disabled = new Set(set.disabled);
    if (set.public) state.pub = { ...state.pub, ...set.public };
    calls.length = 0; logs.length = 0;
    let results = null;
    if (step.op === 'parallel') {
      let release;
      gate.open = step.gate ? new Promise((resolve) => { release = resolve; }) : Promise.resolve();
      const started = step.ops.map((op, i) => {
        // Forced calls are started a tick after the unforced ones, as a caller arriving while one is running.
        return new Promise((resolve) => setImmediate(resolve)).then(() => (op === 'force' ? wiring.discoverMcpTools(true) : wiring.discoverMcpTools()));
      });
      await new Promise((resolve) => setTimeout(resolve, 20));
      const during = calls.length;
      if (release) release();
      await Promise.all(started);
      gate.open = Promise.resolve();
      results = { listingsBeforeRelease: during };
    } else {
      gate.open = Promise.resolve();
      await wiring.discoverMcpTools(step.op === 'force');
    }
    const sortedCalls = [...calls].sort((a, b) => (a.server < b.server ? -1 : a.server > b.server ? 1 : 0) || String(JSON.stringify(a.headers)).localeCompare(String(JSON.stringify(b.headers))));
    const now = summary(step === scenario.steps[scenario.steps.length - 1]);
    const same = previous !== null && JSON.stringify(previous) === JSON.stringify(now);
    previous = now;
    steps.push({ at: clock.now, op: step.op, ops: step.ops || null, calls: sortedCalls, parallel: results, state: same ? { same: true } : now, logs: [...logs] });
  }
  return { steps, facts: factsDoc, picker: await pickerFacts() };
}

(async () => {
  const doc = {
    version: 1,
    manifests: { off: buildToolboxManifest({ diaryMcpWrite: false }), on: buildToolboxManifest({ diaryMcpWrite: true }) },
    scenarios: [],
  };
  for (const scenario of SCENARIOS) {
    doc.scenarios.push({
      name: scenario.name,
      diaryMcpWrite: !!scenario.diaryMcpWrite,
      servers: scenario.servers || [],
      tools: scenario.tools || {},
      fail: scenario.fail || {},
      dir: scenario.dir || [],
      ready: scenario.ready || {},
      disabled: scenario.disabled || [],
      noToken: !!scenario.noToken,
      script: scenario.steps,
      ...(await run(scenario)),
    });
  }
  fs.writeFileSync(out, `{"version":1,"manifests":${JSON.stringify(doc.manifests)},"scenarios":[\n${doc.scenarios.map((x) => JSON.stringify(x)).join(',\n')}\n]}\n`);
})().catch((e) => { console.error(e && e.stack || e); process.exit(1); });
