#!/usr/bin/env node
'use strict';
// Recorded provider call path for crates/chat-upstream: the contract src/target.rs and src/refusal.rs are replayed against.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/chat-upstream/tests/record_upstream.cjs > crates/chat-upstream/tests/fixtures/node-upstream.v1.json
//   ... node crates/chat-upstream/tests/record_upstream.cjs --check crates/chat-upstream/tests/fixtures/node-upstream.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every answer is computed by noevia-core's own code over synthetic, QA-named inputs:
//   server/chat.cjs              the upstreamUrl template, the memberOwnProvider expression, the sign-in block and the
//                                member-origin refusal, each cut out of the source by an exact pattern (the recorder
//                                fails when the source no longer has that shape) and run as written
//   server/ssrf.cjs              createEndpointApproved (MEMBER_OUTBOUND_ORIGINS from the injected env)
//   server/providers.cjs         createProviderRegistry().providerHeaders (the bearer rule)
//   server/chat-machines.cjs     createChatMachines().provider (the row of a turn on an Inference machine)
//   server/rust-chat-native.cjs  refusal() (its provider and provider-key checks)
// The source files' sha256 are recorded.

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const core = path.resolve(checkout);
const FILES = ['server/chat.cjs', 'server/ssrf.cjs', 'server/providers.cjs', 'server/chat-machines.cjs', 'server/rust-chat-native.cjs'];
const SOURCES = {};
for (const rel of FILES) SOURCES[rel] = crypto.createHash('sha256').update(fs.readFileSync(path.join(core, rel))).digest('hex');
for (const level of ['log', 'info', 'warn', 'error']) console[level] = (...args) => process.stderr.write(`${args.map(String).join(' ')}\n`);

const chatSource = fs.readFileSync(path.join(core, 'server/chat.cjs'), 'utf8');
function cut(re, what) {
  const m = chatSource.match(re);
  if (!m) { process.stderr.write(`chat.cjs no longer has the expected ${what}; update the recorder and the port.\n`); process.exit(1); }
  return m;
}
const urlTemplate = cut(/\n {4}const upstreamUrl = `([^`]+)`;\n/, 'upstreamUrl template')[1];
const upstreamUrlOf = new Function('provider', `return \`${urlTemplate}\`;`);
const memberOwnExpr = cut(/\n {4}const memberOwnProvider = ([^;\n]+);\n/, 'memberOwnProvider expression')[1];
const memberOwnOf = new Function('authn', 'provider', 'DEFAULT_PROVIDER_ID', `return (${memberOwnExpr});`);
const originBlock = cut(/\n {4}(if \(memberOwnProvider && !chatgptProvider && !endpointApproved\(authn, upstreamUrl\)\) \{\n {6}return json\(res, 400, \{ error: '[^'\n]+' \}\);\n {4}\})\n/, 'member-origin refusal')[1];
const originCheck = new Function('memberOwnProvider', 'chatgptProvider', 'endpointApproved', 'authn', 'upstreamUrl', 'json', 'res', `${originBlock}\nreturn null;`);
const signInBlock = cut(/\n {4}let providerFetch = fetch;\n {4}(if \(chatgptProvider\) \{\n[\s\S]*?\n {6}providerFetch = chatgptOAuth\.fetchFor\(ownerId, \{ conversation: chatId \|\| spaceId \|\| null \}\);\n {4}\})\n/, 'sign-in block')[1];
const signInCheck = new Function('chatgptProvider', 'chatgptOAuth', 'chatgptEnabled', 'authn', 'provider', 'chatWorkspace', 'chatId', 'spaceId', 'json', 'res', 'fetch',
  `let providerFetch = fetch;\n${signInBlock}\nreturn { fetch: providerFetch };`);

const { createEndpointApproved } = require(path.join(core, 'server/ssrf.cjs'));
const { createProviderRegistry } = require(path.join(core, 'server/providers.cjs'));
const { createChatMachines } = require(path.join(core, 'server/chat-machines.cjs'));
const native = require(path.join(core, 'server/rust-chat-native.cjs'));

const DEFAULT = 'default';
const json = (_res, status, body) => ({ status, error: body.error });

// ── Inputs (synthetic, QA-named) ─────────────────────────────────────────
const BASES = ['http://qa-engine:8080', 'http://qa-engine:8080/', 'http://qa-engine:8080/v1', 'http://qa-engine:8080/v1/',
  'https://qa.example/api/v1', 'http://qa-engine/v1/v1', 'http://qa-engine//', 'http://qa-engine/V1', 'http://qa-engine/v1x',
  '', '/', 'v1', '/v1', 'http://qa-engine/v1?x=1', 'http://qa-user:qa-pass@qa-engine/v1', 'ftp://qa-engine', 'http://[::1]:8080/v1',
  'https://QA.EXAMPLE/', 'http://qa.example:80/', 'https://qa.example:443/v1', 'https://qa.example:8443/', 'http://qa.example./',
  'not a url', 'http://qa-engine/v1\n', 'https://qa.example/v1#frag', 'HTTP://qa.example/v1', 'file:///qa/v1', 'http://qa.example',
  'http://qa-engine:8080/v1//', 'https://ʀqa.example/'];
const ORIGINS = ['', 'http://qa.example', 'https://qa.example, https://qa.example:8443', ' http://qa-engine:8080 ,http://qa-engine', 'https://qa.example:443', '*'];
const USERS = [null, { id: 'qa-admin', admin: true }, { id: 'qa-member', admin: false }, { id: '', admin: false }];
const ROWS = (base) => [
  { id: DEFAULT, label: 'QA default', baseUrl: base, apiKey: 'qa-key', shared: true },
  { id: 'qa-shared', label: 'QA shared', baseUrl: base, apiKey: 'qa-key', shared: true },
  { id: 'qa-own', label: 'QA own', baseUrl: base, apiKey: '', shared: false },
  { id: 'qa-own-key', label: 'QA own keyed', baseUrl: base, apiKey: 'qa-key' },
];

function authnOf(u) { return u ? { user: { id: u.id, role: u.admin ? 'admin' : 'member' } } : null; }

/** chat.cjs's order: the sign-in block (provider as getProvider gave it), the machine swap, the address, the origin rule. */
function nodeTarget(c) {
  let provider = c.row;
  let chatgptProvider = !!c.signIn;
  const authn = authnOf(c.user);
  const oauth = { status: () => ({ state: c.connection }), fetchFor: (owner, o) => ({ broker: owner, conversation: o.conversation }) };
  const chatWorkspace = c.workspaceUser === undefined ? {} : { userId: c.workspaceUser };
  const s = signInCheck(chatgptProvider, oauth, () => !!c.signInEnabled, authn, provider, chatWorkspace, c.chatId, c.spaceId, json, null, 'direct');
  if (s.status) return { refused: { status: s.status, error: s.error } };
  let fetch = s.fetch;
  if (c.machine) {
    const machines = createChatMachines({ machines: [] });
    provider = machines.provider({ machine: { id: c.machine, name: 'QA box' }, role: 'smart', baseUrl: c.row.baseUrl, port: 8080, model: 'qa-model' });
    chatgptProvider = false;
    fetch = 'machine';
  }
  // A machine turn's row is the machine's (recorded, so the replay starts from the same row).
  const row = c.machine ? { row: provider } : {};
  let upstreamUrl;
  try { upstreamUrl = upstreamUrlOf(provider); } catch { return { ...row, throws: true }; }
  const memberOwnProvider = memberOwnOf(authn, provider, DEFAULT);
  const approved = createEndpointApproved({ env: { MEMBER_OUTBOUND_ORIGINS: c.origins } });
  const o = originCheck(memberOwnProvider, chatgptProvider, approved, authn, upstreamUrl, json, null);
  if (o) return { ...row, refused: { status: o.status, error: o.error } };
  const out = { ...row, url: upstreamUrl, providerId: provider.id, label: provider.label, memberOwn: !!memberOwnProvider };
  if (fetch === 'machine') out.fetch = { machine: c.machine };
  else if (fetch && typeof fetch === 'object') out.fetch = { broker: fetch.broker, conversation: fetch.conversation };
  else out.fetch = 'direct';
  return out;
}

const targets = [];
for (const base of BASES) for (const user of USERS) for (const row of ROWS(base)) for (const origins of ORIGINS) {
  const c = { row, user, origins };
  targets.push({ case: c, node: nodeTarget(c) });
}
// The non-text address (Node throws building the URL).
for (const baseUrl of [undefined, null, 7]) targets.push({ case: { row: { id: 'qa-own', label: 'QA odd', baseUrl }, user: USERS[2], origins: '' }, node: nodeTarget({ row: { id: 'qa-own', label: 'QA odd', baseUrl }, user: USERS[2], origins: '' }) });
// Turns on an Inference machine (the row becomes the machine's; shared, no key).
for (const base of ['http://qa-box:8080', 'http://qa-box:8080/', 'https://qa-box.example']) for (const user of USERS) {
  const c = { row: ROWS(base)[0], user, origins: '', machine: 'qa-m1' };
  targets.push({ case: c, node: nodeTarget(c) });
}
// The sign-in provider.
for (const user of USERS) for (const shared of [false, true]) for (const signInEnabled of [true, false])
  for (const connection of ['connected', 'reconnect', 'disconnected']) for (const workspaceUser of [undefined, 'qa-member', 'qa-other', ''])
    for (const [chatId, spaceId] of [[null, null], ['qa-chat', null], ['', 'qa-space'], [null, 'qa-space'], ['qa-chat', 'qa-space']]) {
      const row = { id: 'qa-signin', label: 'QA sign-in', baseUrl: 'https://qa-signin.example/v1', apiKey: '', shared };
      const c = { row, user, origins: '', signIn: true, signInEnabled, connection, workspaceUser, chatId, spaceId };
      targets.push({ case: c, node: nodeTarget(c) });
    }

const { providerHeaders } = createProviderRegistry({ PROVIDERS: [], DEFAULT_PROVIDER_ID: DEFAULT });
const headers = [];
for (const apiKey of [undefined, null, '', 'local', 'LOCAL', ' local', 'qa-key', 'Bearer qa', 'qa key with spaces']) {
  const h = providerHeaders({ id: 'qa', apiKey });
  headers.push({ apiKey: apiKey === undefined ? { undefined: true } : apiKey, authorization: h.Authorization === undefined ? null : h.Authorization });
}

const refusals = [];
for (const providerId of [DEFAULT, 'qa-other', undefined]) for (let bits = 0; bits < 32; bits++) {
  const f = { providerId, defaultProviderId: DEFAULT, machine: !!(bits & 1), external: !!(bits & 2), chatgpt: !!(bits & 4), routeTarget: (bits & 8) ? { route: 'cloud' } : null, authorized: !!(bits & 16) };
  const t = { ...f, chatIdNotText: false, deleted: false, diary: false, messages: [] };
  refusals.push({ facts: { ...f, providerId: providerId === undefined ? null : providerId, routeTarget: !!f.routeTarget }, node: native.refusal(t) });
}

const out = `${JSON.stringify({ version: 1, sources: SOURCES, targets, headers, refusals }, null, 1)}\n`;
const check = process.argv.indexOf('--check');
if (check !== -1) {
  const file = process.argv[check + 1];
  if (fs.readFileSync(file, 'utf8') !== out) { process.stderr.write(`${file} differs from a fresh recording\n`); process.exit(1); }
  process.stderr.write(`${file} matches (${targets.length} targets, ${headers.length} headers, ${refusals.length} refusals)\n`);
} else process.stdout.write(out);
