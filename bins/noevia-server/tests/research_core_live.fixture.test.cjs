'use strict';
const test = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const path = require('node:path');
const { observeNativeReadiness } = require('./research_core_live.cjs');
const core = process.env.NOEVIA_RESEARCH_CORE;
assert.ok(core, 'an actual paired Core checkout is required');
const { createNativeLeaseClient } = require(path.join(core, 'server/model-maintenance-lease.cjs'));
const { createAuthority, SECRET } = require(path.join(core, 'server/synthetic-lease-authority.test-helper.cjs'));

async function fixture(t, control) {
  const authority = createAuthority();
  const server = http.createServer((req, res) => control(req, res, authority));
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise(resolve => { server.closeAllConnections(); server.close(resolve); }));
  return {
    authority,
    client: createNativeLeaseClient({ port: server.address().port, token: SECRET }),
  };
}

test('a refused actual HTTP registration cannot emit readiness', { timeout: 10000 }, async t => {
  let allow = false, refused;
  const denied = new Promise(resolve => { refused = resolve; });
  const f = await fixture(t, (req, res, authority) => {
    if (!allow && req.url.endsWith('/session')) {
      res.once('finish', refused);
      res.writeHead(503, { 'content-type': 'application/json' });
      return res.end('{}');
    }
    authority.handler(req, res);
  });
  let ready = false;
  const complete = observeNativeReadiness(f.client, () => { ready = true; });
  await denied;
  await new Promise(setImmediate);
  assert.equal(ready, false, 'HTTP refusal must not be mistaken for registration');
  assert.equal(f.authority.calls.length, 0);
  allow = true;
  await complete;
  assert.equal(ready, true);
  assert.ok(f.authority.calls.some(call => call.op === 'session'));
  assert.ok(f.authority.calls.some(call => call.op === 'held'));
  assert.equal(f.authority.acquires(), 0, 'readiness never acquires a lease');
});

test('registration alone does not bypass a real held-front barrier', { timeout: 10000 }, async t => {
  let observed;
  const held = new Promise(resolve => { observed = resolve; });
  const f = await fixture(t, (req, res, authority) => {
    if (req.url.endsWith('/held') && authority.held()?.origin === 'front') res.once('finish', observed);
    authority.handler(req, res);
  });
  f.authority.takeAsFront();
  let ready = false;
  const complete = observeNativeReadiness(f.client, () => { ready = true; });
  await held;
  await new Promise(setImmediate);
  assert.equal(ready, false, 'actual registered front custody keeps the barrier closed');
  f.authority.releaseFront();
  await complete;
  assert.equal(ready, true);
  assert.equal(f.authority.acquires(), 0);
});
