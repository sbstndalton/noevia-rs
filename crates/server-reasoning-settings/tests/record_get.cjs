#!/usr/bin/env node
'use strict';
// Recorded GET /api/reasoning-settings (track R12) for crates/server-reasoning-settings `read` and the front's route.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/server-reasoning-settings/tests/record_get.cjs > crates/server-reasoning-settings/tests/fixtures/node-reasoning-get.v1.json
//   ... node crates/server-reasoning-settings/tests/record_get.cjs --check crates/server-reasoning-settings/tests/fixtures/node-reasoning-get.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every expectation comes from noevia-core's own code: the real route (routes/reasoning-settings.cjs
// createReasoningSettingsRoutes) with the real reasoning-effort.cjs (it reads process.env at call time, set per
// case), the real provider registry (providers.cjs createProviderRegistry over a real workspace store's merged
// `providers`, workspace.cjs) and the account's projects from that store. Glue, checked against the source:
//   - the default provider row is built from the environment with index.cjs's own expressions (each line below is
//     asserted to appear verbatim in server/index.cjs);
//   - `getProject(id)` is `PROJECTS.find((p) => p.id === id) || null` over the store's list (projects.cjs; its
//     self-heal saves do not change what this route reads, and are server-projects' tested `get_project`);
//   - `secrets` is a stand-in whose decrypt is production's answer for a value that is not an envelope
//     (secret-envelope.cjs `open`: `value || ''`); the rows here hold no envelopes. A store WITH secrets assigns
//     each row's key (openProviderKey), which throws for a row that is not an object, and that decides which rows
//     load; a truthy non-string key reaches capabilityKey as it is;
//   - the settings row is a stand-in `db` answering `SELECT value FROM settings WHERE key='reasoning_effort_default'`.
// Inputs are synthetic and QA-named. Each source file's sha256 is recorded. stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => process.stderr.write(`${a.map(String).join(' ')}\n`);
const READ_FILES = ['server/routes/reasoning-settings.cjs', 'server/reasoning-effort.cjs', 'server/providers.cjs', 'server/workspace.cjs', 'server/index.cjs'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex');

// index.cjs's expressions for the default provider, verbatim (asserted below).
const INDEX_LINES = [
  "const INFERENCE_BASE = process.env.INFERENCE_BASE_URL || process.env.LEMONADE_BASE_URL || 'http://host.docker.internal:11434';",
  "const INFERENCE_KEY = process.env.INFERENCE_API_KEY || process.env.LEMONADE_API_KEY || '';",
  "const DEFAULT_PROVIDER_ID = process.env.DEFAULT_PROVIDER_ID || 'default';",
  "const DEFAULT_PROVIDER_LABEL = process.env.DEFAULT_PROVIDER_LABEL || 'Local inference';",
  "const MODEL_MANAGER_KIND = process.env.MODEL_MANAGER_KIND || (process.env.LEMONADE_BASE_URL ? 'lemonade' : 'none');",
  "const DEFAULT_PROVIDER_CAPABILITIES = require('./providers.cjs').engineCapabilities(MODEL_MANAGER_KIND);",
  "const workspaceStore = createWorkspaceStore(DATA_DIR, { id: DEFAULT_PROVIDER_ID, label: DEFAULT_PROVIDER_LABEL, baseUrl: INFERENCE_BASE, apiKey: INFERENCE_KEY, shared: true,",
  "  ...(DEFAULT_PROVIDER_CAPABILITIES ? { capabilities: DEFAULT_PROVIDER_CAPABILITIES } : {}) }, secretStore, { projectStore: PROJECT_STORE,",
  "const reasoningSettingsRoutes = require('./routes/reasoning-settings.cjs').createReasoningSettingsRoutes({ json, authService, getProject, getProvider, reasoningEffort, DEFAULT_PROVIDER_ID });",
];
const indexText = fs.readFileSync(path.join(root, 'server/index.cjs'), 'utf8');
for (const line of INDEX_LINES) if (!indexText.split('\n').includes(line)) throw new Error(`index.cjs no longer has: ${line}`);

const { createReasoningSettingsRoutes } = require(path.join(root, 'server/routes/reasoning-settings.cjs'));
const reasoningEffort = require(path.join(root, 'server/reasoning-effort.cjs'));
const providers = require(path.join(root, 'server/providers.cjs'));
const { createWorkspaceStore } = require(path.join(root, 'server/workspace.cjs'));

const USER = 'bbbbbbbb-2222-4bbb-8bbb-bbbbbbbbbbbb';
const ENV_KEYS = ['INFERENCE_BASE_URL', 'LEMONADE_BASE_URL', 'INFERENCE_API_KEY', 'LEMONADE_API_KEY', 'DEFAULT_PROVIDER_ID', 'DEFAULT_PROVIDER_LABEL', 'MODEL_MANAGER_KIND'];
const LOCAL = { MODEL_MANAGER_KIND: 'llamacpp', INFERENCE_BASE_URL: 'http://qa-engine:8080/v1' };
const p = (fields) => JSON.stringify(Object.assign({ id: 'proj-qa-r', name: 'QA reasoning', modes: ['chat'] }, fields));
const projectsOf = (...items) => `{"projects":[${items.join(',')}]}`;
const rowsOf = (...items) => `{"providers":[${items.join(',')}]}`;
const SHARED_CAPS = JSON.stringify({ id: 'shared-qa', label: 'QA shared', baseUrl: 'http://qa-shared:1/v1', capabilities: { reasoningEffortParam: true, reasoningEffortModels: [' qa-think ', 'qa-other'] } });
const PRIVATE_DOC = JSON.stringify({ id: 'mine-qa', label: 'QA mine', baseUrl: 'http://qa-mine:1/v1', capabilities: { reasoningEffortParam: true } });

// Each case: { name, env?, setting? (undefined: no row), query?, projects?, shared?, private?, role? }.
const cases = [];
const add = (name, c) => cases.push({ name, ...c });
// The account default and no project.
add('no-row', {});
for (const v of ['low', 'high', 'default', '', 'bogus', 'LOW']) add(`setting-${v || 'empty'}`, { setting: v });
add('admin', { role: 'admin', setting: 'high' });
// ?projectId.
add('project-missing', { query: '?projectId=nope', projects: projectsOf(p({})) });
add('project-empty-param', { query: '?projectId', projects: projectsOf(p({})) });
add('project-empty-value', { query: '?projectId=', projects: projectsOf(p({ id: '' })) });
add('project-encoded', { query: '?projectId=proj%2Dqa+r%20x', projects: projectsOf(p({ id: 'proj-qa r x', reasoningEffort: 'low' })) });
add('project-first-value', { query: '?projectId=proj-qa-r&projectId=other', projects: projectsOf(p({ reasoningEffort: 'high' })) });
add('project-bad-escape', { query: '?projectId=%E0%A4%A', projects: projectsOf(p({ id: '�%A', reasoningEffort: 'low' })) });
add('project-no-file', { query: '?projectId=proj-qa-r' });
add('other-param-only', { query: '?projectid=proj-qa-r', projects: projectsOf(p({ reasoningEffort: 'high' })) });
for (const e of ['low', 'high', 'default', 'High', 5, null]) add(`project-effort-${String(e)}`, { setting: 'low', query: '?projectId=proj-qa-r', projects: projectsOf(p({ reasoningEffort: e })) });
add('project-internal-id', { query: '?projectId=cowork-diary-extras', projects: projectsOf(p({ id: 'cowork-diary-extras', reasoningEffort: 'high' })) });
// Providers and capabilities.
const withProvider = (name, fields, extra = {}) => add(name, { setting: 'high', query: '?projectId=proj-qa-r', projects: projectsOf(p(fields)), ...extra });
withProvider('default-row-no-caps', {});
withProvider('shared-row-documented', { provider: 'shared-qa', model: 'qa-think' }, { shared: rowsOf(SHARED_CAPS) });
withProvider('shared-row-model-not-listed', { provider: 'shared-qa', model: 'qa-plain' }, { shared: rowsOf(SHARED_CAPS) });
withProvider('shared-row-model-untrimmed', { provider: 'shared-qa', model: ' qa-think ' }, { shared: rowsOf(SHARED_CAPS) });
withProvider('shared-row-default-effort', { provider: 'shared-qa', model: 'qa-think', reasoningEffort: 'default' }, { shared: rowsOf(SHARED_CAPS) });
withProvider('unknown-provider-falls-back', { provider: 'nobody', model: 'qa-think' }, { shared: rowsOf(SHARED_CAPS) });
withProvider('lemonade-alias', { provider: 'lemonade', model: 'qwen3-qa' }, { env: LOCAL });
withProvider('provider-number', { provider: 7, model: 'qa' }, { shared: rowsOf(JSON.stringify({ id: 7, baseUrl: 'http://qa:1', capabilities: { reasoningEffortParam: true } })) });
withProvider('provider-empty-string', { provider: '', model: 'qwen3-qa' }, { env: LOCAL });
withProvider('private-row', { provider: 'mine-qa', model: 'qa' }, { private: rowsOf(PRIVATE_DOC) });
withProvider('private-row-shared-flag', { provider: 'mine-qa', model: 'qa' }, { private: rowsOf(JSON.stringify({ ...JSON.parse(PRIVATE_DOC), shared: true })) });
withProvider('private-duplicate-first-wins', { provider: 'mine-qa', model: 'qa' }, { private: rowsOf(JSON.stringify({ id: 'mine-qa', capabilities: {} }), PRIVATE_DOC) });
withProvider('private-lemonade-row', { provider: 'lemonade', model: 'qa' }, { private: rowsOf(JSON.stringify({ id: 'lemonade', capabilities: { reasoningEffortParam: true } })) });
withProvider('private-default-id-row', { model: 'qa' }, { private: rowsOf(JSON.stringify({ id: 'default', capabilities: { reasoningEffortParam: true } })) });
withProvider('private-null-row', { provider: 'mine-qa', model: 'qa' }, { private: rowsOf('null', PRIVATE_DOC) });
withProvider('private-number-row', { provider: 'mine-qa', model: 'qa' }, { private: rowsOf('3', PRIVATE_DOC) });
withProvider('private-array-row', { provider: 'mine-qa', model: 'qa' }, { private: rowsOf('[1]', PRIVATE_DOC) });
withProvider('private-providers-string', { provider: 'mine-qa', model: 'qa' }, { private: '{"providers":"ab"}' });
withProvider('private-providers-object', { provider: 'mine-qa', model: 'qa' }, { private: '{"providers":{"a":1}}' });
withProvider('private-doc-null', { provider: 'mine-qa', model: 'qa' }, { private: 'null' });
withProvider('private-malformed', { provider: 'mine-qa', model: 'qa' }, { private: '{"providers":[' });
withProvider('shared-damaged-null-row', { provider: 'shared-qa', model: 'qa-think' }, { shared: rowsOf(SHARED_CAPS, 'null') });
withProvider('shared-damaged-array-row', { provider: 'shared-qa', model: 'qa-think' }, { shared: rowsOf(SHARED_CAPS, '[]') });
withProvider('shared-damaged-not-list', { provider: 'shared-qa', model: 'qa-think' }, { shared: '{"providers":{}}' });
withProvider('shared-providers-null', { provider: 'shared-qa', model: 'qa-think' }, { shared: '{"providers":null}' });
withProvider('shared-top-array', { provider: 'shared-qa', model: 'qa-think' }, { shared: '[]' });
withProvider('shared-malformed', { provider: 'shared-qa', model: 'qa-think' }, { shared: '{' });
withProvider('shared-stale-default-row', { model: 'qa' }, { shared: rowsOf(JSON.stringify({ id: 'default', capabilities: { reasoningEffortParam: true } })) });
withProvider('shared-and-private-same-id', { provider: 'both-qa', model: 'qa' }, { shared: rowsOf(JSON.stringify({ id: 'both-qa', capabilities: {} })), private: rowsOf(JSON.stringify({ id: 'both-qa', capabilities: { reasoningEffortParam: true } })) });
// effectiveCapabilities: stored values re-validated; a row without capabilities inherits the preset's.
const capsCase = (name, caps, model = 'qa') => withProvider(`caps-${name}`, { provider: 'caps-qa', model }, { shared: rowsOf(JSON.stringify({ id: 'caps-qa', baseUrl: 'http://qa:1', ...(caps === undefined ? {} : { capabilities: caps }) })) });
capsCase('empty', {});
capsCase('null', null);
capsCase('array', []);
capsCase('unknown-key', { reasoningEffortParam: true, extra: 1 });
capsCase('param-string', { reasoningEffortParam: 'true' });
capsCase('param-false', { reasoningEffortParam: false });
capsCase('models-not-list', { reasoningEffortParam: true, reasoningEffortModels: 'qa' });
capsCase('models-empty-list', { reasoningEffortParam: true, reasoningEffortModels: [] });
capsCase('models-blank', { reasoningEffortParam: true, reasoningEffortModels: [' '] });
capsCase('models-trimmed-unicode', { reasoningEffortParam: true, reasoningEffortModels: ['　qa﻿'] });
capsCase('models-too-long', { reasoningEffortParam: true, reasoningEffortModels: ['q'.repeat(201)] });
capsCase('models-200-units', { reasoningEffortParam: true, reasoningEffortModels: ['😀'.repeat(100)] }, '😀'.repeat(100));
capsCase('models-201-units', { reasoningEffortParam: true, reasoningEffortModels: ['😀'.repeat(100) + 'q'] }, '😀'.repeat(100) + 'q');
capsCase('models-51', { reasoningEffortParam: true, reasoningEffortModels: Array.from({ length: 51 }, (_, i) => `m${i}`) });
capsCase('models-number-entry', { reasoningEffortParam: true, reasoningEffortModels: ['qa', 1] });
capsCase('budget-field-bad', { reasoningEffortParam: true, tokenBudgetField: 'tokens' });
capsCase('json-schema-bad', { reasoningEffortParam: true, jsonSchemaParam: 1 });
capsCase('all-valid', { reasoningEffortParam: true, reasoningEffortModels: ['qa'], tokenBudgetField: 'max_completion_tokens', jsonSchemaParam: false });
const presetCase = (name, baseUrl, model = 'gpt-5.4') => withProvider(`preset-${name}`, { provider: 'preset-qa', model }, { shared: rowsOf(JSON.stringify({ id: 'preset-qa', ...(baseUrl === undefined ? {} : { baseUrl }) })) });
presetCase('root', 'https://api.openai.com');
presetCase('slash', 'https://api.openai.com/');
presetCase('v1', 'https://api.openai.com/v1');
presetCase('v1-slash', 'https://api.openai.com/v1/');
presetCase('v2', 'https://api.openai.com/v2');
presetCase('query', 'https://api.openai.com/v1?x=1');
presetCase('empty-query', 'https://api.openai.com/v1?');
presetCase('hash', 'https://api.openai.com/v1#a');
presetCase('empty-hash', 'https://api.openai.com/v1#');
presetCase('user', 'https://qa@api.openai.com/v1');
presetCase('port-443', 'https://api.openai.com:443/v1');
presetCase('port-other', 'https://api.openai.com:8443/v1');
presetCase('upper-host', 'HTTPS://API.OPENAI.COM/v1');
presetCase('http', 'http://api.openai.com/v1');
presetCase('other-model', 'https://api.openai.com/v1', 'gpt-qa');
presetCase('no-base', undefined);
presetCase('base-array', ['https://api.openai.com']);
presetCase('base-number', 5);
presetCase('base-bad', 'not a url');
// The local engine's switch.
const localCase = (name, env, model, baseUrl) => withProvider(`local-${name}`, { model, ...(baseUrl ? { provider: 'eng-qa' } : {}) }, { env, ...(baseUrl ? { shared: rowsOf(JSON.stringify({ id: 'eng-qa', baseUrl })) } : {}) });
localCase('qwen3', LOCAL, 'qwen3-qa');
localCase('qwen3.5', LOCAL, 'org/Qwen3.5_qa');
localCase('qwen3-no-sep', LOCAL, 'qwen3');
localCase('qwen30', LOCAL, 'qwen30-qa');
localCase('xqwen3', LOCAL, 'xqwen3-qa');
localCase('lemonade-kind', { ...LOCAL, MODEL_MANAGER_KIND: 'lemonade' }, 'qwen3-qa');
localCase('kind-none', { ...LOCAL, MODEL_MANAGER_KIND: 'none' }, 'qwen3-qa');
localCase('kind-unset-lemonade-url', { LEMONADE_BASE_URL: 'http://qa-engine:8080/v1', INFERENCE_BASE_URL: 'http://qa-engine:8080/v1' }, 'qwen3-qa');
localCase('inference-unset', { MODEL_MANAGER_KIND: 'llamacpp', LEMONADE_BASE_URL: 'http://qa-engine:8080/v1' }, 'qwen3-qa');
localCase('trailing-slashes', { MODEL_MANAGER_KIND: 'llamacpp', INFERENCE_BASE_URL: 'http://qa-engine:8080/v1///' }, 'qwen3-qa');
localCase('host-case', { MODEL_MANAGER_KIND: 'llamacpp', INFERENCE_BASE_URL: 'http://QA-ENGINE:8080/v1' }, 'qwen3-qa', 'http://qa-engine:8080/v1/');
localCase('default-port', { MODEL_MANAGER_KIND: 'llamacpp', INFERENCE_BASE_URL: 'http://qa-engine:80/v1' }, 'qwen3-qa', 'http://qa-engine/v1');
localCase('other-row-other-url', LOCAL, 'qwen3-qa', 'http://elsewhere-qa:1/v1');
localCase('inference-invalid', { MODEL_MANAGER_KIND: 'llamacpp', INFERENCE_BASE_URL: 'qa engine' }, 'qwen3-qa');
localCase('model-number', LOCAL, 3);
localCase('default-id-custom', { ...LOCAL, DEFAULT_PROVIDER_ID: 'qa-local' }, 'qwen3-qa');
localCase('default-id-empty', { ...LOCAL, DEFAULT_PROVIDER_ID: '' }, 'qwen3-qa');
withProvider('machine-switch-row', { provider: 'mach-qa', model: 'qa' }, { shared: rowsOf(JSON.stringify({ id: 'mach-qa', baseUrl: 'http://qa:1', machine: { thinkingSwitch: true } })) });
withProvider('machine-switch-truthy', { provider: 'mach-qa', model: 'qa' }, { shared: rowsOf(JSON.stringify({ id: 'mach-qa', baseUrl: 'http://qa:1', machine: { thinkingSwitch: 1 } })) });
withProvider('llamacpp-default-caps', { model: 'qa' }, { env: { MODEL_MANAGER_KIND: 'llamacpp' } });
// Values with lone surrogates and escapes travel unchanged.
add('setting-unicode', { setting: 'hé"\\' });
// A projects.json Node's load reads as empty: getProject finds nothing.
add('project-file-malformed', { query: '?projectId=proj-qa-r', projects: '{' });
add('project-file-projects-object', { query: '?projectId=proj-qa-r', projects: '{"projects":{}}' });
// Node's throws on hand-edited rows and projects (its handler answers 500).
withProvider('apikey-number-real', { provider: 'key-qa', model: 'qa' }, { shared: rowsOf(JSON.stringify({ id: 'key-qa', baseUrl: 'http://qa:1', apiKey: 5, capabilities: { reasoningEffortParam: true } })) });
withProvider('apikey-number-hint', { provider: 'key-qa', model: 'qa' }, { shared: rowsOf(JSON.stringify({ id: 'key-qa', baseUrl: 'http://qa:1', apiKey: 5, capabilities: {} })) });
withProvider('apikey-string-real', { provider: 'key-qa', model: 'qa' }, { shared: rowsOf(JSON.stringify({ id: 'key-qa', baseUrl: 'http://qa:1', apiKey: 'qa-plain-key', capabilities: { reasoningEffortParam: true } })) });
withProvider('apikey-own-tostring-real', { provider: 'key-qa', model: 'qa' }, { shared: rowsOf(JSON.stringify({ id: 'key-qa', baseUrl: 'http://qa:1', apiKey: { toString: 1 }, capabilities: { reasoningEffortParam: true } })) });
withProvider('apikey-array-real', { provider: 'key-qa', model: 'qa' }, { shared: rowsOf(JSON.stringify({ id: 'key-qa', baseUrl: 'http://qa:1', apiKey: ['qa'], capabilities: { reasoningEffortParam: true } })) });
withProvider('model-own-tostring-local', { model: { toString: 1 } }, { env: LOCAL });
withProvider('model-own-tostring-not-local', { model: { toString: 1 } });
withProvider('model-own-tostring-machine', { provider: 'mach-qa', model: { toString: 1 } }, { shared: rowsOf(JSON.stringify({ id: 'mach-qa', baseUrl: 'http://qa:1', machine: { thinkingSwitch: true } })) });
// Fail-closed cases (put in the fixture as `difference`): files this port cannot hold.
withProvider('shared-huge-number', { provider: 'shared-qa', model: 'qa-think' }, { shared: rowsOf(SHARED_CAPS, '{"id":"x","n":1e999}'), difference: 'unholdable-providers' });
withProvider('private-huge-number', { provider: 'mine-qa', model: 'qa' }, { private: rowsOf(PRIVATE_DOC.replace('}', ',"n":1e999}')), difference: 'unholdable-providers' });

async function record(c) {
  for (const k of ENV_KEYS) delete process.env[k];
  Object.assign(process.env, c.env || {});
  // index.cjs, evaluated now for this case's environment.
  const INFERENCE_BASE = process.env.INFERENCE_BASE_URL || process.env.LEMONADE_BASE_URL || 'http://host.docker.internal:11434';
  const INFERENCE_KEY = process.env.INFERENCE_API_KEY || process.env.LEMONADE_API_KEY || '';
  const DEFAULT_PROVIDER_ID = process.env.DEFAULT_PROVIDER_ID || 'default';
  const DEFAULT_PROVIDER_LABEL = process.env.DEFAULT_PROVIDER_LABEL || 'Local inference';
  const MODEL_MANAGER_KIND = process.env.MODEL_MANAGER_KIND || (process.env.LEMONADE_BASE_URL ? 'lemonade' : 'none');
  const DEFAULT_PROVIDER_CAPABILITIES = providers.engineCapabilities(MODEL_MANAGER_KIND);
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-qa-reasoning-get-'));
  const dir = path.join(data, 'users', USER);
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  if (c.projects !== undefined) fs.writeFileSync(path.join(dir, 'projects.json'), c.projects);
  if (c.private !== undefined) fs.writeFileSync(path.join(dir, 'providers.json'), c.private);
  if (c.shared !== undefined) fs.writeFileSync(path.join(data, 'shared-providers.json'), c.shared);
  // secret-envelope.cjs `open` for a value that is not an envelope: `String(value || '')` first (it throws for an own
  // toString), then `value || ''`.
  const secrets = { decrypt: (v) => { String(v || ''); return v || ''; } };
  const projectStore = { async read() { throw new Error('unused'); }, async write() { throw new Error('unused'); } };
  const store = createWorkspaceStore(data, { id: DEFAULT_PROVIDER_ID, label: DEFAULT_PROVIDER_LABEL, baseUrl: INFERENCE_BASE, apiKey: INFERENCE_KEY, shared: true,
    ...(DEFAULT_PROVIDER_CAPABILITIES ? { capabilities: DEFAULT_PROVIDER_CAPABILITIES } : {}) }, secrets, { projectStore });
  const ws = store.get(USER);
  const { getProvider } = providers.createProviderRegistry({ PROVIDERS: ws.providers, DEFAULT_PROVIDER_ID });
  const getProject = (id) => ws.projects.find((pr) => pr.id === id) || null;
  const db = { prepare: (sql) => ({ get: () => {
    if (sql !== "SELECT value FROM settings WHERE key='reasoning_effort_default'") throw new Error(`unexpected SQL ${sql}`);
    return c.setting === undefined ? undefined : { value: c.setting };
  } }) };
  const res = {};
  const route = createReasoningSettingsRoutes({ json: (r, status, body) => { res.status = status; res.body = JSON.stringify(body); return true; }, authService: { db }, getProject, getProvider, reasoningEffort, DEFAULT_PROVIDER_ID });
  const query = c.query || '';
  const url = new URL(`/api/reasoning-settings${query}`, 'http://qa-host');
  let threw = null;
  try { await route({ method: 'GET', headers: {} }, res, { path: '/api/reasoning-settings', authn: { user: { id: USER, role: c.role || 'member' } }, url }); }
  catch (e) { threw = String(e && e.message || e); }
  fs.rmSync(data, { recursive: true, force: true });
  const { name, difference, ...input } = c;
  return { name, input: { ...input, query }, ...(difference ? { difference } : {}), expected: { status: res.status ?? null, reply: res.body ?? null, threw } };
}

(async () => {
  const results = [];
  for (const c of cases) results.push(await record(c));
  const fixture = JSON.stringify({
    generator: 'crates/server-reasoning-settings/tests/record_get.cjs',
    user: USER,
    sources: Object.fromEntries(READ_FILES.map((f) => [f, sha(f)])),
    cases: results,
  }, null, 1) + '\n';
  const check = process.argv.indexOf('--check');
  if (check !== -1) {
    if (fs.readFileSync(process.argv[check + 1], 'utf8') !== fixture) { process.stderr.write('reasoning GET fixture differs from a fresh recording\n'); process.exit(1); }
    process.stderr.write(`reasoning GET fixture matches (${results.length} cases)\n`);
    return;
  }
  process.stdout.write(fixture);
})().catch((e) => { process.stderr.write(`${e.stack || e}\n`); process.exit(1); });
