'use strict';
// Run against the pre-move Core checkout (fixtures/reasoning-get-oracle.core-ref): the actual GET
// /api/reasoning-settings route over the actual reasoning-effort.cjs and provider registry. The settings table, the
// projects and the providers are synthetic. Records the answer and what `modeFor` was asked, so the native route's
// bridge call can be checked against it. Usage: CORE_ROOT=<core> node reasoning-get-oracle.cjs > out.json
const { Readable } = require('node:stream');
const CORE = process.env.CORE_ROOT;
const { createReasoningSettingsRoutes } = require(CORE + '/server/routes/reasoning-settings.cjs');
const reasoningEffort = require(CORE + '/server/reasoning-effort.cjs');
const { createProviderRegistry } = require(CORE + '/server/providers.cjs');

const PROVIDERS = [
  { id: 'default', label: 'Default', baseUrl: 'http://engine.invalid/v1', apiKey: 'k0', capabilities: {} },
  { id: 'real', label: 'Real', baseUrl: 'http://api.invalid/v1', apiKey: 'k1', capabilities: { reasoningEffortParam: true } },
  { id: 'scoped', label: 'Scoped', baseUrl: 'http://api2.invalid/v1', apiKey: 'k2', capabilities: { reasoningEffortParam: true, reasoningEffortModels: ['m-ok'] } },
];
const { getProvider } = createProviderRegistry({ PROVIDERS, DEFAULT_PROVIDER_ID: 'default' });
const p = (id, extra = {}) => ({ id, name: id, files: [], modes: ['chat'], chats: [], ...extra });
const PROJECTS = [
  p('plain'), p('hi', { reasoningEffort: 'high' }), p('lo', { reasoningEffort: 'low', provider: 'real', model: 'any' }),
  p('def', { reasoningEffort: 'default', provider: 'real', model: 'any' }), p('bad', { reasoningEffort: 'max', provider: 'real', model: 'any' }),
  p('upper', { reasoningEffort: 'HIGH' }), p('null-effort', { reasoningEffort: null, provider: 'scoped', model: 'm-ok' }),
  p('scoped-ok', { provider: 'scoped', model: 'm-ok' }), p('scoped-other', { provider: 'scoped', model: 'other' }),
  p('ghost-provider', { provider: 'ghost', model: 'm-ok' }), p('legacy-provider', { provider: 'lemonade', model: 'm' }),
  p('empty-provider', { provider: '', model: '' }), p('number-provider', { provider: 5, model: 7 }), p('object-provider', { provider: { id: 'real' }, model: ['m'] }),
  p('null-provider', { provider: null, model: null }), p('qwen-local', { provider: 'default', model: 'qwen3-8b' }),
  p('dup', { reasoningEffort: 'low' }), p('dup', { reasoningEffort: 'high' }), p('café ☕'), p('a b'), p('a+b'), p('50%'),
];
const SETTINGS = [
  ['missing', undefined], ['default', 'default'], ['low', 'low'], ['high', 'high'], ['empty', ''], ['null', null], ['unknown', 'max'], ['upper', 'LOW'],
];
(async () => {
  const cases = [];
  const queries = [
    ['none', ''], ['plain', '?projectId=plain'], ['hi', '?projectId=hi'], ['lo', '?projectId=lo'], ['def', '?projectId=def'], ['bad', '?projectId=bad'],
    ['upper', '?projectId=upper'], ['null-effort', '?projectId=null-effort'], ['scoped-ok', '?projectId=scoped-ok'], ['scoped-other', '?projectId=scoped-other'],
    ['ghost-provider', '?projectId=ghost-provider'], ['legacy-provider', '?projectId=legacy-provider'], ['empty-provider', '?projectId=empty-provider'],
    ['number-provider', '?projectId=number-provider'], ['object-provider', '?projectId=object-provider'], ['null-provider', '?projectId=null-provider'],
    ['qwen-local', '?projectId=qwen-local'], ['dup', '?projectId=dup'], ['unknown', '?projectId=ghost'], ['empty-id', '?projectId='], ['bare', '?projectId'],
    ['encoded', '?projectId=caf%C3%A9%20%E2%98%95'], ['plus-is-space', '?projectId=a+b'], ['literal-plus', '?projectId=a%2Bb'], ['percent', '?projectId=50%25'],
    ['first-wins', '?projectId=hi&projectId=lo'], ['first-unknown', '?projectId=ghost&projectId=hi'], ['other-param', '?other=1'], ['case-sensitive', '?projectid=hi'],
    ['trailing-amp', '?projectId=hi&'], ['bad-percent', '?projectId=%ZZ'],
  ];
  for (const [skind, svalue] of SETTINGS) for (const [qname, search] of queries) {
    if (skind !== 'low' && skind !== 'missing' && !['none', 'plain', 'hi', 'def', 'bad', 'scoped-ok', 'unknown', 'dup'].includes(qname)) continue;
    for (const role of ['member', 'admin']) {
      if (role === 'admin' && !['none', 'plain', 'hi', 'unknown'].includes(qname)) continue;
      const asked = [];
      const sent = [];
      const routes = createReasoningSettingsRoutes({
        json: (_res, status, body) => sent.push({ status, body }),
        authService: { db: { prepare: () => ({ get: () => (svalue === undefined ? undefined : { value: svalue }) }) } },
        getProject: (id) => PROJECTS.find((x) => x.id === id) || null,
        getProvider,
        reasoningEffort: { ...reasoningEffort, modeFor: (provider, model, effort) => { asked.push({ provider: provider ? provider.id : null, model, effort }); return reasoningEffort.modeFor(provider, model, effort); } },
        DEFAULT_PROVIDER_ID: 'default',
      });
      const req = Readable.from([]); req.method = 'GET';
      await routes(req, {}, { path: '/api/reasoning-settings', authn: { user: { id: 'u', role } }, url: new URL('http://localhost/api/reasoning-settings' + search) });
      cases.push({ name: `${skind}/${qname}/${role}`, settings: skind === 'missing' ? null : { kind: skind, value: svalue }, search, role, reply: sent[0], asked });
    }
  }
  process.stdout.write(JSON.stringify({ node: process.version, projects: PROJECTS, cases }, null, 1));
})().catch((e) => { console.error(e); process.exitCode = 1; });
