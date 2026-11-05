'use strict';
// Residual method/path/auth/feature grid for the N2 (auth/profile/admin), N3 (integrations,
// connectors, MCP, storage) and N4 (models, providers, settings) route families.
//
//   node residual-grid-oracle.cjs run <base> <data-dir> <routes.toml> <out.json>
//   node residual-grid-oracle.cjs merge <pass1.json> <pass2.json> <fixture.json>
//
// `run` drives a fresh stack started by tools/front/replay-front.sh (the front with Node at
// contracts/http/front-core.ref behind it, synthetic data dir) the way a browser would: it
// completes first-run setup as a synthetic admin, invites and signs up a synthetic member, then
// sends every grid case and records status and body. `merge` takes two runs on two fresh stacks
// and marks the cases whose body differs between them (tokens, timestamps) as volatile, so the
// Rust replay (tests/residual_grid.rs) compares status only for those.
//
// Synthetic accounts and data only. Cases that would perform a native write with valid
// credentials are left out: they are each route's own tests, not residual fallbacks.
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');

const ORIGIN = 'http://127.0.0.1:18021';
const HOST = '127.0.0.1:18021';
const METHODS = ['GET', 'HEAD', 'POST', 'PUT', 'PATCH', 'DELETE', 'OPTIONS'];
const UNSAFE = ['POST', 'PUT', 'PATCH', 'DELETE'];
// N2, N3, N4 families (Diary, projects, chats and research are other items).
const FAMILIES = [
  ['N2', /^\/api\/(account|admin\/(invitations|users|web-address|features)|instance|auth|profile|setup|features|chat-framing\/preferences|chat-vault-mirror)(\/|$)/],
  ['N3', /^\/api\/(connectors|integrations\/storage|mcp-keys|mcp-oauth|admin\/mcp-directory|admin\/offsite-backup|plugins\/directory|toolboxes)(\/|$|\{)/],
  ['N4', /^\/api\/(models|model-manager\/models|providers|sampling-settings|reasoning-settings|routing-mode|routing-default|admin\/decision-settings|auto-roles|usage|stats)(\/|$)/],
];

function rustRoutes(toml) {
  const out = [];
  for (const block of fs.readFileSync(toml, 'utf8').split(/^\[\[route\]\]\s*$/m).slice(1)) {
    const field = (k) => block.match(new RegExp(`^${k}\\s*=\\s*(.+)$`, 'm'))?.[1].trim();
    if (field('owner') !== '"rust"') continue;
    const p = JSON.parse(field('path'));
    const methods = field('methods') ? JSON.parse(field('methods')) : [];
    const family = FAMILIES.find(([, re]) => re.test(p));
    if (family) out.push({ family: family[0], path: p, methods });
  }
  return out;
}

// Concrete paths for one route pattern: well-formed, trailing slash, malformed identifier.
function variants(pattern) {
  const optional = /\{\*\?\}$/;
  const base = pattern.replace(optional, '');
  const has = /\{[^}]*\}/.test(base);
  const fill = (v) => base.replace(/\{[^}]*\}/g, v);
  const out = [['plain', fill('grid-x')], ['slash', `${fill('grid-x')}/`]];
  if (has) out.push(['malformed', fill('%zz')], ['dotted', fill('..')], ['encoded-slash', fill('a%2Fb')]);
  if (optional.test(pattern)) out.push(['suffix', `${base}/grid-x`]);
  return out;
}

function cases(toml) {
  const list = [];
  const seen = new Set();
  for (const r of rustRoutes(toml)) {
    for (const [shape, p] of variants(r.path)) {
      for (const method of METHODS) {
        const native = r.methods.length === 0 || r.methods.includes(method);
        for (const who of ['anon', 'member', 'member-nocsrf', 'admin', 'admin-nocsrf']) {
          const csrf = who === 'member' || who === 'admin';
          // Signed out: every method (the outer authentication gate). Signed in without the
          // CSRF token: unsafe methods only (a safe method needs none). Signed in with it: only
          // methods the route does not list natively (the residual fallback itself).
          if (who.endsWith('-nocsrf') && !UNSAFE.includes(method)) continue;
          if (csrf && native) continue;
          const id = `${method} ${p} ${who}`;
          if (seen.has(id)) continue;
          seen.add(id);
          list.push({ id, family: r.family, route: r.path, shape, method, path: p, who });
        }
      }
    }
  }
  return list;
}

function request(base, method, p, headers, body) {
  const u = new URL(base);
  return new Promise((resolve, reject) => {
    // One connection per case, with an explicit length: a reused keep-alive socket after a HEAD
    // or a chunked DELETE body made the stack answer stray empty 400s.
    const sized = body === undefined ? {} : { 'content-length': String(Buffer.byteLength(body)) };
    const req = http.request({ host: u.hostname, port: u.port, method, path: p, agent: false, headers: { host: HOST, connection: 'close', ...sized, ...headers } }, (res) => {
      const chunks = [];
      res.on('data', (c) => chunks.push(c));
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, text: Buffer.concat(chunks).toString('utf8') }));
      res.on('error', reject);
    });
    req.setTimeout(30000, () => req.destroy(new Error(`timeout ${method} ${p}`)));
    req.on('error', reject);
    if (body !== undefined) req.write(body);
    req.end();
  });
}

function jar(res) {
  const cookies = (res.headers['set-cookie'] || []).map((c) => c.split(';')[0]);
  const csrf = cookies.find((c) => c.startsWith('cowork_csrf='))?.split('=')[1];
  if (!csrf || cookies.length < 2) throw new Error(`no session cookies (${res.status} ${res.text})`);
  return { cookie: cookies.join('; '), csrf };
}

async function post(base, p, json, extra = {}) {
  return request(base, 'POST', p, { origin: ORIGIN, 'content-type': 'application/json', ...extra }, JSON.stringify(json));
}

// The same steps tests/residual_grid.rs takes against the in-process front.
async function accounts(base, dataDir) {
  const code = fs.readFileSync(path.join(dataDir, 'first-run-setup-code'), 'utf8').trim();
  const setup = await post(base, '/api/setup/complete', { setupCode: code, username: 'grid-admin', displayName: 'Grid Admin', password: 'residual-grid-Synthetic-pw-1', diaryEnabled: false });
  if (setup.status !== 201) throw new Error(`setup ${setup.status} ${setup.text}`);
  const admin = jar(setup);
  const invite = await post(base, '/api/admin/invitations', { role: 'member' }, { cookie: admin.cookie, 'x-csrf-token': admin.csrf });
  if (invite.status !== 201) throw new Error(`invite ${invite.status} ${invite.text}`);
  const accept = await post(base, '/api/auth/invitations/accept', { token: JSON.parse(invite.text).token, username: 'grid-member', displayName: 'Grid Member', password: 'residual-grid-Synthetic-pw-2', diaryEnabled: false });
  if (accept.status !== 201) throw new Error(`accept ${accept.status} ${accept.text}`);
  return { admin, member: jar(accept) };
}

function headersFor(c, ids) {
  const h = { origin: ORIGIN };
  const acct = c.who.startsWith('admin') ? ids.admin : c.who.startsWith('member') ? ids.member : null;
  if (acct) h.cookie = acct.cookie;
  if (acct && !c.who.endsWith('-nocsrf')) h['x-csrf-token'] = acct.csrf;
  if (UNSAFE.includes(c.method)) h['content-type'] = 'application/json';
  return h;
}

function parseBody(text) {
  if (text === '') return { kind: 'empty' };
  try { return { kind: 'json', json: JSON.parse(text) }; } catch { return { kind: 'text', text }; }
}

async function run(base, dataDir, toml, out) {
  const ids = await accounts(base, dataDir);
  const results = [];
  for (const c of cases(toml)) {
    // Node's outermost gate (index.cjs device-auth -> features.enabled) briefly fails every
    // request with 503 FEATURE_RUNTIME_CONTEXT_UNAVAILABLE ("Internal error") while it cannot
    // read the front's feature runtime context. That is a transient of the stack, not the
    // case's answer: retry it and count the retries (reported, not hidden).
    let res;
    let retries = 0;
    for (;;) {
      res = await request(base, c.method, c.path, headersFor(c, ids), UNSAFE.includes(c.method) ? '{}' : undefined);
      if (!(res.status === 503 && res.text === '{"error":"Internal error"}') || retries === 20) break;
      retries += 1;
      await new Promise((r) => setTimeout(r, 250));
    }
    results.push({ ...c, status: res.status, body: parseBody(res.text), ...(retries ? { transient503: retries } : {}) });
  }
  fs.writeFileSync(out, `${JSON.stringify(results)}\n`);
  console.log(`${results.length} cases -> ${out}`);
}

function merge(a, b, out) {
  const one = JSON.parse(fs.readFileSync(a, 'utf8'));
  const two = new Map(JSON.parse(fs.readFileSync(b, 'utf8')).map((c) => [c.id, c]));
  const rows = one.map((c) => {
    const d = two.get(c.id);
    if (!d || d.status !== c.status) throw new Error(`unstable status for ${c.id}: ${c.status} vs ${d?.status}`);
    const volatile = JSON.stringify(d.body) !== JSON.stringify(c.body);
    const { transient503: _t, ...row } = c;
    return volatile ? { ...row, body: null, volatile: true } : row;
  });
  const doc = {
    note: 'Generated by tests/residual-grid-oracle.cjs: two runs on fresh synthetic stacks (tools/front/replay-front.sh, Node at contracts/http/front-core.ref). Status and body each case got; volatile = body differed between the runs (compare status only).',
    frontCoreRef: fs.readFileSync(path.join(__dirname, '../../../contracts/http/front-core.ref'), 'utf8').trim(),
    cases: rows,
  };
  // One case per line: reviewable diffs on regeneration.
  const headLines = Object.entries(doc).filter(([k]) => k !== 'cases').map(([k, v]) => ` ${JSON.stringify(k)}: ${JSON.stringify(v)},\n`).join('');
  fs.writeFileSync(out, `{\n${headLines} "cases": [\n${rows.map((r) => `  ${JSON.stringify(r)}`).join(',\n')}\n ]\n}\n`);
  console.log(`${rows.length} cases, ${rows.filter((r) => r.volatile).length} volatile -> ${out}`);
}

const [mode, ...args] = process.argv.slice(2);
(mode === 'run' ? run(...args) : mode === 'merge' ? Promise.resolve(merge(...args)) : Promise.reject(new Error('usage: run|merge')))
  .catch((e) => { console.error(e.stack || e.message); process.exitCode = 1; });
