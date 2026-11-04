'use strict';
// Test-only residual-method oracle. Executes Core's actual remaining route mounts. The outer
// session gate mirrors index.cjs; requests use synthetic browser sessions and valid CSRF.
// Deliberately no store, model, worker or production server is started. Unsupported methods
// remain delegated: this test does not transfer route ownership or retire the fallback.
const path = require('node:path');
const fs = require('node:fs');
const http = require('node:http');
const [core, portFile] = process.argv.slice(2);
const load = file => require(path.join(core, 'server', file));
const json = (res, status, body) => {
  res.writeHead(status, { 'Content-Type': 'application/json' });
  res.end(JSON.stringify(body));
};
const forbidden = () => { throw new Error('negative method reached a substantive operation'); };
const mounts = [
  load('routes/account.cjs').createAccountRoutes({ json }),
  load('routes/auth.cjs').createAuthRoutes({ json, readJson: forbidden }).account,
  load('routes/storage.cjs').createStorageRoutes({ json }),
  load('routes/providers.cjs').createProviderRoutes({ json, chatgptOAuth: {}, chatgptEnabled: () => true }),
  load('routes/reasoning-settings.cjs').createReasoningSettingsRoutes({ json }),
  load('routes/sampling-settings.cjs').createSamplingSettingsRoutes({ json }),
  load('routes/models.cjs').createModelRoutes({ json, readBody: forbidden, readJson: forbidden,
    fetchJson: forbidden, env: {}, modelManager: {}, currentWorkspace: () => ({}),
    service: { modelScanCache: { clear() {} } } }),
];
const fallback = load('spa-routes.cjs').createStaticFallback({ json, staticFiles: { resolve: () => null } });
const server = http.createServer(async (req, res) => {
  try {
    const url = new URL(req.url, 'http://localhost');
    const p = url.pathname;
    if (load('rust-auth.cjs').refuseOwned(req, res, p, json)) return;
    const cookie = req.headers.cookie || '';
    const role = cookie.includes('admin-token') ? 'admin' : cookie.includes('member-token') ? 'member' : null;
    if (!role) return load('http.cjs').unauthorized(res);
    const authn = { user: { id: 'synthetic', role } };
    // Feature-off uses a separate mount, without changing any deployment feature flag.
    if (p.startsWith('/api/providers/chatgpt') && req.headers['x-test-feature-off'] === '1') {
      const off = load('routes/providers.cjs').createProviderRoutes({ json });
      if (await off(req, res, { path: p, authn, url })) return;
    }
    for (const mount of mounts) if (await mount(req, res, { path: p, authn, url })) return;
    fallback(req, res, p);
  } catch (e) { json(res, 500, { error: e.message }); }
});
server.listen(0, '127.0.0.1', () => fs.writeFileSync(portFile, String(server.address().port)));
setTimeout(() => server.close(), 120000).unref();
