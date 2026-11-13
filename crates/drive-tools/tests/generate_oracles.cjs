#!/usr/bin/env node
'use strict';
// Records Node oracles for crates/drive-tools and crates/mcp-client from noevia-core's own
// server/gdrive-tools.cjs and server/mcp.cjs.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> node crates/drive-tools/tests/generate_oracles.cjs
//
// Writes crates/drive-tools/tests/fixtures/drive-oracle.v1.json and
// crates/mcp-client/tests/fixtures/mcp-oracle.v1.json. Synthetic data only. Nothing here touches the
// network: the Drive file module is replaced by a stub before gdrive-tools.cjs is loaded, and mcp.cjs
// is only used for its pure helpers. Deterministic (controlled clock, no random ids). The source
// files' sha256 are checked: a changed Node file needs a fresh recording.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const Module = require('node:module');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const serverDir = path.join(path.resolve(checkout), 'server');
const SOURCES = {
  'server/gdrive-tools.cjs': '4829829e378cac40f888ea169c2793c0e1605268246771968c37b9fb8ff5a2c9',
  'server/mcp.cjs': '8a20cc410642344d6feb73ecfe661cc100f5649d79fe6ceee1661f725e9aa077',
};
for (const [rel, want] of Object.entries(SOURCES)) {
  const got = crypto.createHash('sha256').update(fs.readFileSync(path.join(checkout, rel))).digest('hex');
  if (got !== want) {
    process.stderr.write(`source hash mismatch for ${rel}: ${got}\n`);
    process.exit(1);
  }
}
const crates = path.join(__dirname, '..', '..');

// ── Stub the Drive file module (no network) ───────────────────────────────
const S = { readReply: null, fail: null, link: null, lastUpdate: null, calls: [] };
const fakeFiles = {
  search: async (args) => {
    S.calls.push(['search', args]);
    if (args.query === 'none') return [];
    return [
      { id: 'a1', name: 'Plan', mimeType: 'text/plain', size: '12', modifiedTime: '2026-11-01T10:00:00Z' },
      { id: 'b2', name: 'Notes', mimeType: '', size: '', webViewLink: 'https://example.test/b2' },
    ];
  },
  recent: async (args) => { S.calls.push(['recent', args]); return S.recentEmpty ? [] : [{ id: 'c3', name: 'Recent', mimeType: 'application/json', size: '0', modifiedTime: '2026-11-02T08:00:00Z' }]; },
  metadata: async (args) => {
    S.calls.push(['metadata', args]);
    return { id: args.fileId, name: 'Plan', mimeType: 'text/plain', size: '12', modifiedTime: '2026-11-01T10:00:00Z', webViewLink: S.link };
  },
  read: async (args) => {
    S.calls.push(['read', args]);
    if (S.fail) throw S.fail;
    return S.readReply;
  },
  create: async (args) => { S.calls.push(['create', args]); return { id: 'n9', name: args.name, webViewLink: S.link, version: '1' }; },
  update: async (args, opts) => {
    S.calls.push(['update', args, opts]);
    S.lastUpdate = { args, opts };
    if (S.fail) throw S.fail;
    return { id: args.fileId, name: 'Plan', version: '8' };
  },
  trash: async (args) => { S.calls.push(['trash', args]); return { id: args.fileId, name: 'Plan' }; },
};
// Every stub answer is kept, so each recorded case carries the data the reply was built from.
for (const key of Object.keys(fakeFiles)) {
  const inner = fakeFiles[key];
  fakeFiles[key] = async (...callArgs) => {
    try {
      if (S.fail) throw S.fail;
      const value = await inner(...callArgs);
      S.last = { value };
      return value;
    } catch (error) {
      S.last = { error: { publicMessage: error.publicMessage === undefined ? null : error.publicMessage } };
      throw error;
    }
  };
}
const fakeFilePath = path.join(serverDir, 'gdrive-files.cjs');
const stubModule = new Module(fakeFilePath);
stubModule.filename = fakeFilePath;
stubModule.loaded = true;
stubModule.exports = { driveFiles: () => fakeFiles, quote: (s) => s };
require.cache[fakeFilePath] = stubModule;

const { createDriveTools } = require(path.join(serverDir, 'gdrive-tools.cjs'));
const mcp = require(path.join(serverDir, 'mcp.cjs'));

// ── Drive oracle ──────────────────────────────────────────────────────────
const NOW = { t: 1_000_000 };
let connected = true;
const accounts = {
  forUser: () => ({ drive: { state: () => ({ state: connected ? 'connected' : 'disconnected' }) } }),
};
const tools = createDriveTools({ accounts, cap: 60, now: () => NOW.t });
const USER = { id: 'user-1' };

const cases = [];
const normalizePlan = (r) => {
  if (r === null) return { kind: 'null' };
  if (r.error !== undefined) return { error: r.error };
  return { target: r.target, kind: r.kind };
};

// Pure describeTarget cases (no network on any of these paths).
const planInputs = [
  { name: 'drive_create_file', args: { name: '  notes.md \n', content: 'x' } },
  { name: 'drive_create_file', args: { name: '   ', content: 'x' } },
  { name: 'drive_create_file', args: { name: 42, content: 'x' } },
  { name: 'drive_create_file', args: { name: 'n'.repeat(300), content: 'x' } },
  { name: 'drive_search_files', args: { query: 'x' } },
  { name: 'drive_read_file', args: { fileId: 'abc' } },
  { name: 'drive_trash_file', args: { fileId: 'noevia projects/x/notes.md' } },
  { name: 'drive_trash_file', args: { fileId: '  ' } },
  { name: 'drive_trash_file', args: {} },
  { name: 'drive_update_file', args: { fileId: 'a'.repeat(201), content: 'x' } },
  { name: 'drive_update_file', args: { fileId: 'bad"id\n', content: 'x' } },
  { name: 'drive_unknown', args: { fileId: 'x' } },
];
const planCases = [];
// describeTarget is async; collected below.

const exec = async (name, args, scope = {}) => {
  S.calls.length = 0;
  S.lastUpdate = null;
  S.last = null;
  const now = NOW.t;
  const wasConnected = connected;
  const out = await tools.execute(USER, name, args, scope);
  const rec = { op: 'execute', tool: name, args, scope, now, connected: wasConnected, stub: S.last, expect: out };
  if (S.lastUpdate) rec.expectVersion = S.lastUpdate.opts && S.lastUpdate.opts.expectVersion;
  return rec;
};

(async () => {
  for (const p of planInputs) {
    planCases.push({
      op: 'plan',
      tool: p.name,
      args: p.args,
      expect: normalizePlan(await tools.describeTarget(USER, p.name, p.args)),
    });
  }
  cases.push(...planCases);

  // Replies from the stubbed file module.
  cases.push(await exec('drive_search_files', { query: 'plan' }));
  cases.push(await exec('drive_search_files', { query: 'none' }));
  S.recentEmpty = true;
  cases.push(await exec('drive_list_recent', {}));
  S.recentEmpty = false;
  cases.push(await exec('drive_list_recent', {}));
  S.link = 'https://example.test/a1';
  cases.push(await exec('drive_get_metadata', { fileId: 'a1' }));
  S.link = null;
  cases.push(await exec('drive_get_metadata', { fileId: 'a1' }));
  S.link = 'https://example.test/n9';
  cases.push(await exec('drive_create_file', { name: 'n.md', content: 'x' }));
  S.link = null;
  cases.push(await exec('drive_create_file', { name: 'n.md', content: 'x' }));
  cases.push(await exec('drive_trash_file', { fileId: 'a1' }));

  // Errors.
  S.fail = Object.assign(new Error('quota'), { publicMessage: 'quota' });
  cases.push(await exec('drive_search_files', { query: 'plan' }));
  S.fail = new Error('internal detail');
  cases.push(await exec('drive_search_files', { query: 'plan' }));
  S.fail = null;

  // Not connected.
  connected = false;
  cases.push(await exec('drive_search_files', { query: 'plan' }));
  connected = true;

  // Read cap (60 units) and the read-before-update gate, on one clock.
  const readCases = [];
  S.readReply = { meta: { id: 'a1', name: 'Plan', version: '7' }, text: 'short body', truncated: false };
  NOW.t = 1_000_000;
  readCases.push(await exec('drive_read_file', { fileId: 'a1' }, { exchangeKey: 'ex1', chatKey: 'c1' }));
  // Full read in ex1: the update is allowed and expects version 7.
  readCases.push(await exec('drive_update_file', { fileId: 'a1', content: 'y' }, { exchangeKey: 'ex1', chatKey: 'c1' }));
  // A different exchange has not read it.
  readCases.push(await exec('drive_update_file', { fileId: 'a1', content: 'y' }, { exchangeKey: 'ex2', chatKey: 'c1' }));
  // A partial read (truncated by the cap) blocks the update in the same exchange.
  S.readReply = { meta: { id: 'a1', name: 'Plan', version: '9' }, text: 'x'.repeat(200), truncated: false };
  readCases.push(await exec('drive_read_file', { fileId: 'a1' }, { exchangeKey: 'ex3' }));
  readCases.push(await exec('drive_update_file', { fileId: 'a1', content: 'y' }, { exchangeKey: 'ex3' }));
  // Drive-truncated read is partial even when it fits.
  S.readReply = { meta: { id: 'a1', name: 'Plan', version: '9' }, text: 'tiny', truncated: true };
  readCases.push(await exec('drive_read_file', { fileId: 'a1' }, { exchangeKey: 'ex4' }));
  readCases.push(await exec('drive_update_file', { fileId: 'a1', content: 'y' }, { exchangeKey: 'ex4' }));
  // A read older than 30 minutes no longer counts.
  S.readReply = { meta: { id: 'a1', name: 'Plan', version: '5' }, text: 'old', truncated: false };
  readCases.push(await exec('drive_read_file', { fileId: 'a1' }, { exchangeKey: 'ex5' }));
  NOW.t += 30 * 60 * 1000 + 1;
  readCases.push(await exec('drive_update_file', { fileId: 'a1', content: 'y' }, { exchangeKey: 'ex5' }));
  // Exactly 30 minutes still counts (now - at <= READ_TTL_MS).
  S.readReply = { meta: { id: 'a3', name: 'Edge', version: '4' }, text: 'edge', truncated: false };
  readCases.push(await exec('drive_read_file', { fileId: 'a3' }, { exchangeKey: 'ex8' }));
  NOW.t += 30 * 60 * 1000;
  readCases.push(await exec('drive_update_file', { fileId: 'a3', content: 'y' }, { exchangeKey: 'ex8' }));
  // The chat scope: no exchange key, the chat key decides; a padded id is trimmed before the gate.
  readCases.push(await exec('drive_read_file', { fileId: 'a3' }, { chatKey: 'c9' }));
  readCases.push(await exec('drive_update_file', { fileId: ' a3 ', content: 'y' }, { chatKey: 'c9' }));
  readCases.push(await exec('drive_update_file', { fileId: 'a3', content: 'y' }, { chatKey: 'c8' }));
  // A non-Drive id falls through to the client with no version.
  readCases.push(await exec('drive_update_file', { fileId: 'noevia projects/a.md', content: 'y' }, { exchangeKey: 'ex6' }));
  // A complete read with a non-ASCII name.
  S.readReply = { meta: { id: 'a2', name: 'Plän', version: '3' }, text: 'é'.repeat(20), truncated: false };
  readCases.push(await exec('drive_read_file', { fileId: 'a2' }, { exchangeKey: 'ex7' }));
  cases.push(...readCases);

  const drive = {
    oracle: 'noevia-core server/gdrive-tools.cjs (execute, describeTarget) with server/gdrive-files.cjs stubbed; cap 60; now controlled',
    sources: SOURCES,
    cap: 60,
    cases,
  };

  // ── MCP oracle ──────────────────────────────────────────────────────────
  const rtt = [];
  const resultCases = [
    { content: [{ type: 'text', text: '﻿ first ' }, { type: 'image', data: 'AAAA' }, { type: 'text', text: 'second ' }] },
    { content: [{ type: 'text', text: '\u0085x' }] },
    { content: [{ type: 'text', text: 5 }] },
    { content: [{ type: 'resource_link' }, { foo: 1 }, null, { type: '' }] },
    { content: [], structuredContent: { b: 2, a: 1, nested: { z: [1, 2] } } },
    { content: [{ type: 'text', text: '   ' }], structuredContent: { k: 'v' } },
    { content: [{ type: 'text', text: 'partial' }], structuredContent: { k: 'v' } },
    { content: [{ type: 'text', text: '  bad  ' }], isError: true },
    { content: [], isError: true },
    { content: [], structuredContent: 0 },
    { content: [{ type: 'text', text: '' }], structuredContent: [1, 'two'] },
    {},
    null,
    'a string result',
  ];
  for (const r of resultCases) {
    const structuredJson = r && typeof r === 'object' && r.structuredContent ? JSON.stringify(r.structuredContent) : null;
    rtt.push({ input: r, structured_json: structuredJson, expect: mcp.resultToText(r) });
  }
  const headerCases = [
    { extra: [['Authorization', 'Bearer t'], ['MCP-Session-Id', 'forged'], ['accept', 'text/html'], ['Content-Type', 'x'], ['mcp-protocol-version', 'x'], ['X-Custom', '1']], builtIn: [['MCP-Protocol-Version', mcp.PROTOCOL_VERSION], ['mcp-session-id', 'real']] },
    { extra: [], builtIn: [['MCP-Protocol-Version', mcp.PROTOCOL_VERSION]] },
    { extra: null, builtIn: [['mcp-session-id', 'real']] },
    { extra: [['X-Trace', 'old'], ['X-Other', '2']], builtIn: [['X-Trace', 'new']] },
  ];
  const headers = headerCases.map((c) => {
    const extra = c.extra ? Object.fromEntries(c.extra) : null;
    const builtIn = Object.fromEntries(c.builtIn);
    const out = mcp.withBuiltInHeaders(extra, builtIn);
    return { extra: c.extra, builtIn: c.builtIn, expect: Object.entries(out) };
  });
  // Transport bodies and the tools/list budget, through mcp.cjs with a recording fetch stub.
  // Each page answers a plain JSON-RPC reply; the env budgets are read per listTools call.
  const sent = [];
  const pagesFor = (pages) => async (_url, init) => {
    const body = JSON.parse(init.body);
    sent.push(body);
    const page = body.method === 'tools/list' ? pages[Math.min(sent.filter((b) => b.method === 'tools/list').length - 1, pages.length - 1)] : {};
    if (body.id === undefined) return new Response(null, { status: 202 });
    return new Response(JSON.stringify({ jsonrpc: '2.0', id: body.id, result: page }), { status: 200, headers: { 'content-type': 'application/json' } });
  };
  const tool = (name, pad = 0) => ({ name, description: 'd'.repeat(pad), inputSchema: { type: 'object' } });
  const listCases = [
    { env: {}, pages: [{ tools: [tool('a'), tool('b')] }] },
    { env: {}, pages: [{ tools: [tool('a')], nextCursor: 'p2' }, { tools: [tool('b')], nextCursor: '' }] },
    { env: {}, pages: [{ tools: [tool('a')], nextCursor: 'p2' }, { tools: [tool('b')] }] },
    { env: { MCP_LIST_TOOLS_MAX_PAGES: '2' }, pages: [{ tools: [tool('a')], nextCursor: 'p2' }, { tools: [tool('b')], nextCursor: 'p3' }, { tools: [tool('c')] }] },
    { env: { MCP_LIST_TOOLS_MAX_PAGES: '1.9' }, pages: [{ tools: [tool('a')], nextCursor: 'p2' }, { tools: [tool('b')] }] },
    { env: { MCP_LIST_TOOLS_MAX_PAGES: '0' }, pages: [{ tools: [tool('a')], nextCursor: 'p2' }, { tools: [tool('b')] }] },
    { env: { MCP_LIST_TOOLS_MAX_BYTES: '120' }, pages: [{ tools: [tool('a'), tool('b'), tool('c'), tool('d')] }] },
    { env: { MCP_LIST_TOOLS_MAX_BYTES: '200' }, pages: [{ tools: [tool('a', 10), tool('é', 30)], nextCursor: 'p2' }, { tools: [tool('z')] }] },
    { env: { MCP_LIST_TOOLS_MAX_BYTES: '-5', MCP_LIST_TOOLS_MAX_PAGES: 'x' }, pages: [{ tools: [tool('a')], nextCursor: 'p2' }, { tools: [] }] },
    { env: {}, pages: [{ tools: null, nextCursor: 'p2' }, {}] },
  ];
  const list = [];
  for (const c of listCases) {
    sent.length = 0;
    const saved = { MCP_LIST_TOOLS_MAX_PAGES: process.env.MCP_LIST_TOOLS_MAX_PAGES, MCP_LIST_TOOLS_MAX_BYTES: process.env.MCP_LIST_TOOLS_MAX_BYTES };
    delete process.env.MCP_LIST_TOOLS_MAX_PAGES; delete process.env.MCP_LIST_TOOLS_MAX_BYTES;
    Object.assign(process.env, c.env);
    const origWarn = console.warn; console.warn = () => {};
    const kept = await mcp.listTools('http://mcp.synthetic.invalid/mcp', { id: 's1' }, {}, 1000, undefined, pagesFor(c.pages));
    console.warn = origWarn;
    for (const [k, v] of Object.entries(saved)) { if (v === undefined) delete process.env[k]; else process.env[k] = v; }
    list.push({ env: c.env, pages: c.pages, requests: sent.map((b) => b.params), kept: kept.map((t) => t.name), tool_bytes: c.pages.flatMap((p) => (p.tools || []).map((t) => [t.name, Buffer.byteLength(JSON.stringify(t))])) });
  }
  sent.length = 0;
  await mcp.connect('http://mcp.synthetic.invalid/mcp', {}, 1000, undefined, pagesFor([{}]));
  const callArgs = [{ q: 1 }, null, undefined, 0, '', false, [], { }];
  for (const a of callArgs) await mcp.callTool('http://mcp.synthetic.invalid/mcp', { id: 's1' }, 'qa_tool', a, {}, 1000, undefined, pagesFor([{}]));
  const bodies = sent.map(({ id, ...rest }) => ({ has_id: id !== undefined, ...rest }));
  const envValues = [undefined, '', ' ', '12', ' 12 ', '12.9', '0', '-1', '1e3', '0x10', '0b11', '0o7', 'Infinity', '-Infinity', 'NaN', 'abc', '5.', '.5', '+7', '1_000', '\u00a012', '\ufeff12', '\u008512', '9007199254740993', '1e400'];
  const positiveEnvSrc = fs.readFileSync(path.join(serverDir, 'mcp.cjs'), 'utf8').match(/function positiveEnv\(name, fallback\) \{[\s\S]*?\n\}/)[0];
  const positiveEnv = new Function('process', positiveEnvSrc + '\nreturn positiveEnv;');
  const envCases = envValues.map((v) => ({ value: v === undefined ? null : v, out: positiveEnv({ env: v === undefined ? {} : { K: v } })('K', 7) }));
  const mcpOracle = {
    oracle: 'noevia-core server/mcp.cjs resultToText and withBuiltInHeaders',
    sources: SOURCES,
    protocol_version: mcp.PROTOCOL_VERSION,
    result_to_text: rtt,
    built_in_headers: headers,
    list_tools: list,
    request_bodies: bodies,
    positive_env: envCases,
  };

  fs.writeFileSync(path.join(crates, 'drive-tools', 'tests', 'fixtures', 'drive-oracle.v1.json'), `${JSON.stringify(drive, null, 2)}\n`);
  fs.writeFileSync(path.join(crates, 'mcp-client', 'tests', 'fixtures', 'mcp-oracle.v1.json'), `${JSON.stringify(mcpOracle, null, 2)}\n`);
  process.stdout.write(`recorded ${cases.length} drive cases and ${rtt.length + headers.length} mcp cases\n`);
})().catch((e) => {
  process.stderr.write(`${e && e.stack ? e.stack : e}\n`);
  process.exit(1);
});
