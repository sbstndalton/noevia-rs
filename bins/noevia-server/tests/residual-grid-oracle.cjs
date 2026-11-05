'use strict';
// Residual method/path/auth/feature grid for the N2 (auth/profile/admin), N3 (integrations,
// connectors, MCP, storage) and N4 (models, providers, settings) route families.
//
//   node residual-grid-oracle.cjs run <base> <data-dir> <routes.toml> <out.json> [--dry]
//   node residual-grid-oracle.cjs merge <pass1.json> <pass2.json> <fixture.json>
//   node residual-grid-oracle.cjs classify <fixture.json> <rust-report.json>
//
// `run` drives a fresh stack started by tools/front/replay-front.sh (the front with Node at
// contracts/http/front-core.ref behind it, synthetic data dir) the way a browser would: it
// completes first-run setup as a synthetic admin, invites and signs up a synthetic member, then
// sends every grid case and records status and body. `--dry` ignores SUCCESSFUL_WRITES and lists
// the unsafe cases that succeeded, to review that list. `merge` takes two runs on two fresh stacks,
// normalizes the reviewed VOLATILE fields, refuses any other body difference, applies the
// INTENTIONAL changes and writes the fixture with the whole POLICY. `classify` copies who answered
// each case (front or node) from a reviewed baseline run of tests/residual_grid.rs
// (RESIDUAL_GRID_CLASSIFY=1, RESIDUAL_GRID_REPORT=<file>) into the fixture; the test then asserts it.
//
// Synthetic accounts and data only.
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');

const ORIGIN = 'http://127.0.0.1:18021';
const HOST = '127.0.0.1:18021';
const METHODS = ['GET', 'HEAD', 'POST', 'PUT', 'PATCH', 'DELETE', 'OPTIONS'];
const UNSAFE = ['POST', 'PUT', 'PATCH', 'DELETE'];
const ROLES = ['anon', 'member', 'member-nocsrf', 'admin', 'admin-nocsrf'];
// N2, N3, N4 families (Diary, projects, chats and research are other items). A route belongs to a
// family when its pattern equals a prefix or continues it with `/` or `{`.
const FAMILIES = {
  N2: ['/api/account', '/api/admin/invitations', '/api/admin/users', '/api/admin/web-address', '/api/admin/features', '/api/instance', '/api/auth', '/api/profile', '/api/setup', '/api/features', '/api/chat-framing/preferences', '/api/chat-vault-mirror'],
  N3: ['/api/connectors', '/api/integrations/storage', '/api/mcp-keys', '/api/mcp-oauth', '/api/admin/mcp-directory', '/api/admin/offsite-backup', '/api/plugins/directory', '/api/toolboxes'],
  N4: ['/api/models', '/api/model-manager/models', '/api/providers', '/api/sampling-settings', '/api/reasoning-settings', '/api/routing-mode', '/api/routing-default', '/api/admin/decision-settings', '/api/auto-roles', '/api/usage', '/api/stats'],
};

// Operation policy. Every route x path shape x method x role is sent, except:
// 1. Signed in without the CSRF token, safe methods (GET/HEAD/OPTIONS) are skipped: they need no
//    token, so they are the same request as the signed-in-with-token case.
// 2. START_WORK: operations that would start work or end the grid's own session, never sent
//    with valid credentials (any shape): `[method, route pattern]`.
// 3. SUCCESSFUL_WRITES: unsafe cases that a dry run (`run --dry`) saw succeed (2xx/3xx) and so
//    change synthetic state for later cases; reviewed and listed by case id.
const START_WORK = [
  ['POST', '/api/auth/logout'],
  ['POST', '/api/models/calibration'],
  ['POST', '/api/models/autotune/resume'],
  ['POST', '/api/models/autotune/untuned'],
  ['POST', '/api/models/presets/reload'],
  ['POST', '/api/models/evidence/recheck'],
  ['POST', '/api/models/evidence/import'],
  ['POST', '/api/admin/offsite-backup/run'],
  ['POST', '/api/admin/offsite-backup/copy'],
  ['POST', '/api/admin/offsite-backup/verify'],
  ['POST', '/api/admin/offsite-backup/new-store'],
  ['POST', '/api/admin/offsite-backup/recovery-key'],
  ['POST', '/api/admin/decision-settings/test'],
  ['POST', '/api/integrations/storage/test'],
  ['POST', '/api/providers/test'],
];
// Dry run 2026-11-05 (front at this branch, Node at front-core.ref): 27 unsafe cases succeeded.
// Not listed: POST/PUT/PATCH/DELETE /api/models/downloads admin (200) - Node's models.cjs:786 answers
// any non-GET method with the download list, a read, so they stay in the grid as residual answers.
// Passkey option POSTs create a sign-in/registration challenge; the rest write settings or delete.
const SUCCESSFUL_WRITES = [
  'PUT /api/account/preferences member',
  'PUT /api/account/preferences admin',
  'DELETE /api/admin/decision-settings admin',
  'POST /api/admin/invitations admin',
  'POST /api/admin/offsite-backup/google/disconnect admin',
  'POST /api/auth/login/passkey/options member',
  'POST /api/auth/login/passkey/options admin',
  'POST /api/auth/passkeys/register/options member',
  'POST /api/auth/passkeys/register/options admin',
  'POST /api/connectors/gdrive/disconnect member',
  'POST /api/connectors/gdrive/disconnect admin',
  'POST /api/integrations/storage/nextcloud/cancel member',
  'POST /api/integrations/storage/nextcloud/cancel admin',
  'DELETE /api/mcp-keys/grid-x member',
  'DELETE /api/mcp-keys/grid-x admin',
  'DELETE /api/mcp-oauth/grid-x member',
  'DELETE /api/mcp-oauth/grid-x admin',
  'PATCH /api/profile member',
  'PATCH /api/profile admin',
  'PUT /api/profile/features member',
  'PUT /api/profile/features admin',
  'POST /api/profile/onboarding member',
  'POST /api/profile/onboarding admin',
];
// Reviewed fields that differ on every run (random ids, challenges, tokens), by `METHOD route`.
// `*` matches one array index or key. They are compared by type: a non-empty string or a
// positive number (timestamps), recorded as <volatile:string> / <volatile:number>.
const VOLATILE = {
  'GET /api/instance': ['/json/id'],
  'POST /api/auth/login/passkey/options': ['/json/options/challenge', '/json/challengeToken', '/json/options/allowCredentials/*/id'],
  'GET /api/admin/users': ['/json/users/*/id'],
  'GET /api/auth/session': ['/json/user/id', '/json/csrfToken'],
  'GET /api/profile': ['/json/user/id', '/json/sessions/*/id', '/json/sessions/*/createdAt', '/json/sessions/*/lastSeenAt', '/json/sessions/*/expiresAt'],
  'GET /api/toolboxes/permitted': ['/json/computedAt'],
  'GET /api/usage/aggregate': ['/json/aggregate/checkedAt'],
};
// Native routes whose answer depends on runtime facts Node publishes into the front (feature
// runtime registration, auto-tune/calibration/preset state, model-manager, Diary, MCP and tool
// facts). The in-process replay has no Node, so for these routes `classify` accepts a differing
// answer as `front-node-facts`: the replay still asserts the front answers them (never Node), but
// compares their bodies only in the real stack (this oracle). Reviewed baseline 2026-11-05.
const NODE_FACTS = [
  '/api/admin/features', '/api/admin/features/{name}', '/api/admin/mcp-directory/custom', '/api/admin/mcp-directory/{*}',
  '/api/chat-vault-mirror/preferences', '/api/connectors', '/api/connectors/nextcloud/policy', '/api/features',
  '/api/integrations/storage/nextcloud/poll', '/api/models/assignments', '/api/models/autotune', '/api/models/autotune/cancel',
  '/api/models/autotune/clear', '/api/models/autotune/resume', '/api/models/autotune/settings', '/api/models/autotune/untuned',
  '/api/models/calibration', '/api/models/calibration/cancel', '/api/models/downloads', '/api/models/inference-budget',
  '/api/models/preset', '/api/profile/diary-connectors', '/api/toolboxes', '/api/toolboxes/permitted',
];
// Deliberate changes from the Node-era answer, applied on every regeneration.
const INTENTIONAL = {
  'GET /api/instance/ anon': {
    issue: 'sbstndalton/noevia#2411',
    note: "Node-era stack answered 503 'Web address is unavailable right now.'; the front answers as canonical /api/instance",
    status: 200,
    body: { kind: 'json', json: { id: '<volatile:string>' } },
    volatileKey: 'GET /api/instance',
  },
};
const POLICY = { families: FAMILIES, methods: METHODS, unsafe: UNSAFE, roles: ROLES, startWork: START_WORK, successfulWrites: SUCCESSFUL_WRITES, volatile: VOLATILE, nodeFacts: NODE_FACTS };

function familyOf(p) {
  return Object.keys(FAMILIES).find((f) => FAMILIES[f].some((x) => p === x || p.startsWith(`${x}/`) || p.startsWith(`${x}{`)));
}

function rustRoutes(toml) {
  const out = [];
  for (const block of fs.readFileSync(toml, 'utf8').split(/^\[\[route\]\]\s*$/m).slice(1)) {
    const field = (k) => block.match(new RegExp(`^${k}\\s*=\\s*(.+)$`, 'm'))?.[1].trim();
    if (field('owner') !== '"rust"') continue;
    const p = JSON.parse(field('path'));
    const methods = field('methods') ? JSON.parse(field('methods')) : [];
    const family = familyOf(p);
    if (family) out.push({ family, path: p, methods });
  }
  return out;
}

// Concrete paths for one route pattern: well-formed, trailing slash, malformed identifiers, and
// an optional suffix appended to the filled base. tests/residual_grid.rs computes the same.
function variants(pattern) {
  const optional = /\{\*\?\}$/;
  const base = pattern.replace(optional, '');
  const has = /\{[^}]*\}/.test(base);
  const fill = (v) => base.replace(/\{[^}]*\}/g, v);
  const out = [['plain', fill('grid-x')], ['slash', `${fill('grid-x')}/`]];
  if (has) out.push(['malformed', fill('%zz')], ['dotted', fill('..')], ['encoded-slash', fill('a%2Fb')]);
  if (optional.test(pattern)) out.push(['suffix', `${fill('grid-x')}/grid-x`]);
  for (const [, p] of out) if (/[{}]/.test(p)) throw new Error(`template placeholder left in ${p}`);
  return out;
}

function cases(toml, dry) {
  const list = [];
  const seen = new Set();
  const writes = new Set(dry ? [] : SUCCESSFUL_WRITES);
  for (const r of rustRoutes(toml)) {
    for (const [shape, p] of variants(r.path)) {
      for (const method of METHODS) {
        for (const who of ROLES) {
          const csrf = who === 'member' || who === 'admin';
          if (who.endsWith('-nocsrf') && !UNSAFE.includes(method)) continue;
          if (csrf && START_WORK.some(([m, route]) => m === method && route === r.path)) continue;
          const id = `${method} ${p} ${who}`;
          if (writes.has(id) || seen.has(id)) continue;
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

async function run(base, dataDir, toml, out, flag) {
  const dry = flag === '--dry';
  const ids = await accounts(base, dataDir);
  const results = [];
  for (const c of cases(toml, dry)) {
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
  if (dry) {
    const ok = results.filter((r) => UNSAFE.includes(r.method) && r.status < 400 && (r.who === 'member' || r.who === 'admin'));
    console.log(`successful unsafe cases (${ok.length}):\n${ok.map((r) => `  ${JSON.stringify(r.id)}, // ${r.status}`).join('\n')}`);
  }
}

// Paths in a parsed body (`/json/a/0/b`) whose values differ between two runs.
function differences(x, y, at = '') {
  if (typeof x !== typeof y || Array.isArray(x) !== Array.isArray(y) || (x === null) !== (y === null)) return [at];
  if (x && typeof x === 'object') {
    const keys = new Set([...Object.keys(x), ...Object.keys(y)]);
    return [...keys].flatMap((k) => (k in x && k in y ? differences(x[k], y[k], `${at}/${k}`) : [`${at}/${k}`]));
  }
  return x === y ? [] : [at];
}

const allowed = (pattern, p) => {
  const a = pattern.split('/');
  const b = p.split('/');
  return a.length === b.length && a.every((s, i) => s === '*' || s === b[i]);
};

// Replace the reviewed volatile fields with a typed marker (the Rust replay does the same).
function normalize(body, patterns) {
  const walk = (v, at) => {
    if (patterns.some((p) => allowed(p, at))) {
      if (typeof v === 'string' && v !== '') return '<volatile:string>';
      if (typeof v === 'number' && v > 0) return '<volatile:number>';
      return v;
    }
    if (Array.isArray(v)) return v.map((x, i) => walk(x, `${at}/${i}`));
    if (v && typeof v === 'object') return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, walk(x, `${at}/${k}`)]));
    return v;
  };
  return walk(body, '');
}

function merge(a, b, out) {
  const one = JSON.parse(fs.readFileSync(a, 'utf8'));
  const two = new Map(JSON.parse(fs.readFileSync(b, 'utf8')).map((c) => [c.id, c]));
  const unreviewed = [];
  let retries = 0;
  const rows = one.map((c) => {
    const d = two.get(c.id);
    if (!d || d.status !== c.status) throw new Error(`unstable status for ${c.id}: ${c.status} vs ${d?.status}`);
    retries += (c.transient503 || 0) + (d.transient503 || 0);
    const patterns = VOLATILE[`${c.method} ${c.route}`] || [];
    const extra = differences(c.body, d.body).filter((p) => !patterns.some((q) => allowed(q, p)));
    if (extra.length) unreviewed.push(`${c.id}: ${extra.join(' ')}`);
    const { transient503: _t, ...row } = c;
    if (INTENTIONAL[c.id]) {
      const { issue, note, status, body, volatileKey } = INTENTIONAL[c.id];
      return { ...row, status, body, intentional: `${issue}: ${note}`, nodeEra: { status: c.status, body: c.body }, volatileKey };
    }
    return { ...row, body: normalize(c.body, patterns) };
  });
  if (unreviewed.length) throw new Error(`bodies differ outside the reviewed VOLATILE fields:\n${unreviewed.join('\n')}`);
  for (const id of Object.keys(INTENTIONAL)) if (!rows.some((r) => r.id === id)) throw new Error(`intentional case ${id} not in the grid`);
  writeFixture(out, {
    note: 'Generated by tests/residual-grid-oracle.cjs: two runs on fresh synthetic stacks (tools/front/replay-front.sh, Node at contracts/http/front-core.ref). policy is the operation policy the case set follows; tests/residual_grid.rs recomputes the set from routes.toml. Bodies have the reviewed policy.volatile fields as <volatile:string|number>; answeredBy comes from `classify`.',
    frontCoreRef: fs.readFileSync(path.join(__dirname, '../../../contracts/http/front-core.ref'), 'utf8').trim(),
    transient503Retries: retries,
    policy: POLICY,
    cases: rows,
  });
  console.log(`${rows.length} cases, ${retries} transient 503 retries -> ${out}`);
}

function classify(fixture, report) {
  const doc = JSON.parse(fs.readFileSync(fixture, 'utf8'));
  const baseline = new Map(JSON.parse(fs.readFileSync(report, 'utf8')).cases.map((r) => [r.id, r]));
  const bad = [];
  for (const c of doc.cases) {
    const r = baseline.get(c.id);
    if (!r) throw new Error(`no baseline answer for ${c.id}`);
    let who = r.answeredBy;
    if (who === 'front' && !r.same) {
      if (NODE_FACTS.includes(c.route)) who = 'front-node-facts';
      else bad.push(c.id);
    }
    c.answeredBy = who;
  }
  if (bad.length) throw new Error(`baseline has native mismatches outside NODE_FACTS; fix them first:\n${bad.join('\n')}`);
  writeFixture(fixture, doc);
  const n = (k) => doc.cases.filter((c) => c.answeredBy === k).length;
  console.log(`classified ${doc.cases.length}: front ${n('front')}, front-node-facts ${n('front-node-facts')}, node ${n('node')}`);
}

// One case per line: reviewable diffs on regeneration.
function writeFixture(out, doc) {
  const headLines = Object.entries(doc).filter(([k]) => k !== 'cases').map(([k, v]) => ` ${JSON.stringify(k)}: ${JSON.stringify(v)},\n`).join('');
  fs.writeFileSync(out, `{\n${headLines} "cases": [\n${doc.cases.map((r) => `  ${JSON.stringify(r)}`).join(',\n')}\n ]\n}\n`);
}

const [mode, ...args] = process.argv.slice(2);
const modes = { run, merge: async (...a) => merge(...a), classify: async (...a) => classify(...a) };
(modes[mode] ? modes[mode](...args) : Promise.reject(new Error('usage: run|merge|classify')))
  .catch((e) => { console.error(e.stack || e.message); process.exitCode = 1; });
