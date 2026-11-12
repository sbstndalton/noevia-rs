'use strict';
// Residual method/path/auth/feature grid for the N2 (auth/profile/admin), N3 (integrations,
// connectors, MCP, storage) and N4 (models, providers, settings) route families.
//
//   node residual-grid-oracle.cjs run <base> <data-dir> <routes.toml> <out.json> [--dry]
//   node residual-grid-oracle.cjs merge <pass1.json> <pass2.json> <fixture.json>
//   node residual-grid-oracle.cjs classify <fixture.json> <rust-report.json>
//   node residual-grid-oracle.cjs selftest   (merge negative controls: volatile drift is refused)
//
// `run` drives a fresh stack started by tools/front/replay-front.sh (the front with Node at
// contracts/http/front-core.ref behind it, synthetic data dir) the way a browser would: it
// completes first-run setup as a synthetic admin, invites and signs up a synthetic member, then
// sends every grid case and records status and body. `--dry` ignores SUCCESSFUL_WRITES and lists
// the unsafe cases that succeeded, to review that list. `merge` takes two runs on two fresh stacks,
// normalizes and validates the reviewed VOLATILE fields in both, requires the normalized bodies
// to be equal, applies the
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
  N3: ['/api/connectors', '/api/integrations/storage', '/api/admin/mcp-directory', '/api/admin/offsite-backup', '/api/plugins/directory', '/api/toolboxes'],
  N4: ['/api/models', '/api/model-manager/models', '/api/providers', '/api/sampling-settings', '/api/reasoning-settings', '/api/routing-mode', '/api/routing-default', '/api/admin/decision-settings', '/api/auto-roles', '/api/usage', '/api/stats'],
};

// Operation policy. Every route x path shape x method x role is sent, except:
// 1. Signed in without the CSRF token, safe methods (GET/HEAD/OPTIONS) are skipped: they need no
//    token, so they are the same request as the signed-in-with-token case.
// 2. START_WORK: the exact method/route/role/shape combinations that would start work or end
//    the grid's own session with the grid's `{}` body. Source-reviewed (no work-starting request
//    was sent to find out); `starts` cites where the work starts. All are static routes, so the
//    only shapes are plain and slash.
// 3. SUCCESSFUL_WRITES: unsafe cases that a dry run (`run --dry`) saw succeed and that write
//    state (reviewed); listed by case id.
//
// Source review (rs at this branch, Core at front-core.ref) of what is sent although it looks
// like work:
// - Members get 403 before any work on every admin-only route here: calibration_routes.rs:297,
//   autotune_resume_routes.rs:217, autotune_untuned_routes.rs:39, model_maint/mod.rs:388 (reload,
//   import), model_maint/evidence_recheck.rs:108, offsite_routes.rs:1211,
//   decision_settings_routes.rs:571-572.
// - Slash variants (except logout) are proxied to Node; Node routes on the raw pathname
//   (core index.cjs:1453) and its handlers and front-ownership refusals for these paths compare
//   exactly (routes/models.cjs:494,503,510; rust-offsite-backup.cjs:21; rust-decision-settings.cjs:15;
//   rust-storage-integrations.cjs:24; rust-provider-test.cjs:15), so they fall through to 404 (or
//   models.cjs:213's member 403) and start nothing; the stack also has no model manager
//   (contract-corpus mocks.cjs:90 MODEL_MANAGER_KIND none).
// - POST /api/admin/offsite-backup/recovery-key: 405 (offsite_routes.rs:1227-1229).
// - POST /api/models/calibration with `{}`: a facts read (calibration_routes.rs:307), 404 when
//   calibration is unavailable (:311), else 400 because parse requires confirmPause (:108-110,
//   answered at :345-347), all before the listing/start calls (:366).
// - POST /api/models/autotune/untuned: 405 (autotune_untuned_routes.rs:76-78) before the scan (:84).
// - POST /api/models/evidence/recheck with `{}`: 404 without a native engine (evidence_recheck.rs:125-130),
//   else 400 Choose a model (:141-148), both before the probe task (:163).
// - POST /api/models/evidence/import with `{}`: 404 without a native engine (model_maint/mod.rs:473-478),
//   else 400 Choose a model (:491-493), both before the import task (:499).
// - POST /api/integrations/storage/test with `{}`: the empty baseUrl is refused
//   (storage_integration_routes.rs:1110-1114) or, if approved, yields the unparsable target `/`
//   (:1128-1130), both before the only fetch (:1136).
// - POST /api/providers/test with `{}`: 400 valid baseUrl required (provider_test_routes.rs:397-398)
//   before any probe.
// - POST /api/models/machines with `{}`: the empty action is neither tune nor tuneAbort, so it
//   reaches the registry mutation, where inference_machines.rs mutate refuses it 400 invalidAction
//   inside the write transaction before anything is saved or audited; no probe, DNS or tune starts.
// - POST /api/models/autotune/resume with `{}`: 404 when auto-tune is unavailable
//   (autotune_resume_routes.rs:231), else validate(confirmPause) refuses (:242; Fault::Confirm 400,
//   server-projects autotune_resume.rs:48-51) before prepare (:251) and commit (:282).
// - POST /api/auth/logout/ (slash): account.rs:60 requires p == "/api/auth/logout", so the slash
//   path is passed on and proxied; pinned Core's logout match is exact, so Node answers 404.
const START_WORK = [
  { method: 'POST', route: '/api/auth/logout', roles: ['member', 'admin'], shapes: ['plain'], starts: 'server-account account.rs:60-66 deletes the session' },
  { method: 'POST', route: '/api/models/presets/reload', roles: ['admin'], shapes: ['plain'], starts: 'model_maint/mod.rs:421 spawns reload_presets (`{}` parses as unload false) once a router is configured' },
  { method: 'POST', route: '/api/models/delete', roles: ['admin'], shapes: ['plain'], starts: 'model_maint/mod.rs DELETE arm spawns delete_model::run (a body without a name is 400 and starts nothing)' },
  { method: 'POST', route: '/api/models/load', roles: ['admin'], shapes: ['plain'], starts: 'model_maint/mod.rs LOAD arm runs load_model::run (a body without a name is 400 and starts nothing)' },
  { method: 'POST', route: '/api/models/unload', roles: ['admin'], shapes: ['plain'], starts: 'model_maint/mod.rs UNLOAD arm spawns load_model::run (a body without a name is 400 and starts nothing)' },
  { method: 'POST', route: '/api/models/prune-missing', roles: ['admin'], shapes: ['plain'], starts: 'model_maint/mod.rs PRUNE arm spawns prune_missing::run (the route reads no body) once a router is configured' },
  { method: 'POST', route: '/api/admin/offsite-backup/run', roles: ['admin'], shapes: ['plain'], starts: 'offsite_routes.rs:1265 run_now (body ignored)' },
  { method: 'POST', route: '/api/admin/offsite-backup/verify', roles: ['admin'], shapes: ['plain'], starts: 'offsite_routes.rs:1266 Job::Verify (body ignored)' },
  { method: 'POST', route: '/api/admin/offsite-backup/copy', roles: ['admin'], shapes: ['plain'], starts: 'offsite_routes.rs:1267 copy_now (body ignored)' },
  { method: 'POST', route: '/api/admin/offsite-backup/new-store', roles: ['admin'], shapes: ['plain'], starts: 'offsite_routes.rs:1268 new_store (body ignored)' },
  { method: 'POST', route: '/api/admin/decision-settings/test', roles: ['admin'], shapes: ['plain'], starts: 'decision_settings_routes.rs:589 connection_test, which probes the validated saved URL (:692 health)' },
];
// Dry run 2026-11-05 (front at this branch, Node at front-core.ref): 27 unsafe cases succeeded.
// Not listed: POST/PUT/PATCH/DELETE /api/models/downloads admin (200) - Node's models.cjs:786 answers
// any non-GET method with the download list, a read, so they stay in the grid as residual answers.
// Passkey option POSTs save a challenge (server-account passkeys.rs:193, :403); the rest write
// settings or delete. Not listed: POST /api/integrations/storage/nextcloud/cancel (200
// {cancelled:false}): the empty flow id finds no flow (storage_integration_routes.rs:643-644,
// :1354-1362), nothing is written.
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
// compares their bodies only in the real stack (this oracle). Reviewed baseline 2026-11-05;
// `classify` refuses an entry no differing case needs. The harness seeds what Node writes at
// boot (instance id, credential-encryption key), so routes needing only those are compared.
const NODE_FACTS = [
  '/api/admin/features', '/api/admin/features/{name}', '/api/admin/mcp-directory/custom', '/api/admin/mcp-directory/{*}',
  '/api/chat-vault-mirror/preferences', '/api/connectors', '/api/connectors/nextcloud/policy', '/api/features',
  '/api/models/assignments', '/api/models/autotune', '/api/models/autotune/cancel',
  '/api/models/autotune/clear', '/api/models/autotune/resume', '/api/models/autotune/settings', '/api/models/autotune/untuned',
  '/api/models/calibration', '/api/models/calibration/cancel', '/api/models/downloads', '/api/models/inference-budget', '/api/models/machines',
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
          if (who.endsWith('-nocsrf') && !UNSAFE.includes(method)) continue;
          if (START_WORK.some((e) => e.method === method && e.route === r.path && e.roles.includes(who) && e.shapes.includes(shape))) continue;
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
    // A HEAD answer has no body: a 503 HEAD is the transient only if the same request as GET
    // (safe) shows it. Up to 60 retries, 500 ms apart.
    const transient = async (res) => {
      if (res.status !== 503) return false;
      if (res.text === '{"error":"Internal error"}') return true;
      if (c.method !== 'HEAD' || res.text !== '') return false;
      const probe = await request(base, 'GET', c.path, headersFor({ ...c, method: 'GET' }, ids));
      return probe.status === 503 && probe.text === '{"error":"Internal error"}';
    };
    let res;
    let retries = 0;
    for (;;) {
      res = await request(base, c.method, c.path, headersFor(c, ids), UNSAFE.includes(c.method) ? '{}' : undefined);
      if (retries === 60 || !(await transient(res))) break;
      retries += 1;
      await new Promise((r) => setTimeout(r, 500));
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

// Replace the reviewed volatile fields with a typed marker (the Rust replay does the same) and
// collect the invalid ones: a volatile field must hold a non-empty string or a positive number.
function normalize(body, patterns) {
  const invalid = [];
  const walk = (v, at) => {
    if (patterns.some((p) => allowed(p, at))) {
      if (typeof v === 'string' && v !== '') return '<volatile:string>';
      if (typeof v === 'number' && Number.isFinite(v) && v > 0) return '<volatile:number>';
      invalid.push(`${at}=${JSON.stringify(v)}`);
      return v;
    }
    if (Array.isArray(v)) return v.map((x, i) => walk(x, `${at}/${i}`));
    if (v && typeof v === 'object') return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, walk(x, `${at}/${k}`)]));
    return v;
  };
  return { body: walk(body, ''), invalid };
}

// Both passes' bodies are normalized and validated, and the normalized structures (markers with
// their types, field presence, every stable value) must be equal.
function mergeRows(one, two) {
  const second = new Map(two.map((c) => [c.id, c]));
  const problems = [];
  let retries = 0;
  const rows = one.map((c) => {
    const d = second.get(c.id);
    if (!d || d.status !== c.status) throw new Error(`unstable status for ${c.id}: ${c.status} vs ${d?.status}`);
    retries += (c.transient503 || 0) + (d.transient503 || 0);
    const patterns = VOLATILE[`${c.method} ${c.route}`] || [];
    const [na, nb] = [normalize(c.body, patterns), normalize(d.body, patterns)];
    for (const [pass, n] of [['a', na], ['b', nb]]) if (n.invalid.length) problems.push(`${c.id}: invalid volatile value in pass ${pass}: ${n.invalid.join(' ')}`);
    const drift = differences(na.body, nb.body);
    if (drift.length) problems.push(`${c.id}: bodies differ after normalization: ${drift.join(' ')}`);
    const { transient503: _t, ...row } = c;
    if (INTENTIONAL[c.id]) {
      const { issue, note, status, body, volatileKey } = INTENTIONAL[c.id];
      return { ...row, status, body, intentional: `${issue}: ${note}`, nodeEra: { status: c.status, body: na.body }, volatileKey };
    }
    return { ...row, body: na.body };
  });
  if (problems.length) throw new Error(`merge refused:\n${problems.join('\n')}`);
  for (const id of Object.keys(INTENTIONAL)) if (!rows.some((r) => r.id === id)) throw new Error(`intentional case ${id} not in the grid`);
  return { rows, retries };
}

function merge(a, b, out) {
  const { rows, retries } = mergeRows(JSON.parse(fs.readFileSync(a, 'utf8')), JSON.parse(fs.readFileSync(b, 'utf8')));
  writeFixture(out, {
    note: 'Generated by tests/residual-grid-oracle.cjs: two runs on fresh synthetic stacks (tools/front/replay-front.sh, Node at contracts/http/front-core.ref). policy is the operation policy the case set follows; tests/residual_grid.rs recomputes the set from routes.toml. Bodies have the reviewed policy.volatile fields as <volatile:string|number>, validated in both runs; answeredBy comes from `classify`.',
    frontCoreRef: fs.readFileSync(path.join(__dirname, '../../../contracts/http/front-core.ref'), 'utf8').trim(),
    transient503Retries: retries,
    policy: POLICY,
    cases: rows,
  });
  console.log(`${rows.length} cases, ${retries} transient 503 retries -> ${out}`);
}

// Merge negative controls: volatile drift between the passes must be refused.
function selftest() {
  const row = (json) => ({ id: 'GET /api/instance anon', family: 'N2', route: '/api/instance', shape: 'plain', method: 'GET', path: '/api/instance', who: 'anon', status: 200, body: { kind: 'json', json } });
  const slash = { ...row({ id: 'x' }), id: 'GET /api/instance/ anon', path: '/api/instance/', shape: 'slash', status: 503, body: { kind: 'json', json: { error: 'Web address is unavailable right now.' } } };
  const pair = (a, b) => [[row(a), slash], [row(b), slash]];
  const ok = mergeRows(...pair({ id: 'aaa' }, { id: 'bbb' }));
  if (ok.rows[0].body.json.id !== '<volatile:string>') throw new Error('selftest: valid volatile id was not normalized');
  const controls = [
    ['string -> number', { id: 'aaa' }, { id: 123 }, /bodies differ after normalization/],
    ['missing field', { id: 'aaa' }, {}, /bodies differ after normalization/],
    ['empty string', { id: 'aaa' }, { id: '' }, /invalid volatile value in pass b/],
    ['non-positive number', { id: 0 }, { id: 0 }, /invalid volatile value in pass a/],
    ['stable field drift', { id: 'aaa', x: 1 }, { id: 'bbb', x: 2 }, /bodies differ after normalization: \/json\/x/],
  ];
  for (const [name, a, b, expect] of controls) {
    let message = null;
    try { mergeRows(...pair(a, b)); } catch (e) { message = e.message; }
    if (!message || !expect.test(message)) throw new Error(`selftest ${name}: merge ${message ? `refused differently: ${message}` : 'accepted the drift'}`);
    console.log(`selftest ${name}: refused as expected`);
  }
  console.log('selftest ok');
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
  const unused = NODE_FACTS.filter((r) => !doc.cases.some((c) => c.route === r && c.answeredBy === 'front-node-facts'));
  if (unused.length) throw new Error(`NODE_FACTS routes with no differing case (remove them): ${unused.join(' ')}`);
  writeFixture(fixture, doc);
  const n = (k) => doc.cases.filter((c) => c.answeredBy === k).length;
  console.log(`classified ${doc.cases.length}: front ${n('front')}, front-node-facts ${n('front-node-facts')}, node ${n('node')}`);
}

// One case per line: reviewable diffs on regeneration.
function writeFixture(out, doc) {
  const headLines = Object.entries(doc).filter(([k]) => k !== 'cases').map(([k, v]) => ` ${JSON.stringify(k)}: ${JSON.stringify(v)},\n`).join('');
  fs.writeFileSync(out, `{\n${headLines} "cases": [\n${doc.cases.map((r) => `  ${JSON.stringify(r)}`).join(',\n')}\n ]\n}\n`);
}

// Focused scoped-handler oracle at Core origin/main. Real Node fallback mounts and HTTP
// helpers; synthetic identities and disabled engine only. No server boot, model or external IO.
async function fallbackRecord(core, toml, fixture, out) {
  const cp = require('node:child_process');
  const vm = require('node:vm');
  const coreRef = cp.execFileSync('git', ['-C', core, 'rev-parse', 'origin/main'], { encoding: 'utf8' }).trim();
  const coreHead = cp.execFileSync('git', ['-C', core, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  if (coreHead !== coreRef) throw new Error('fallback-record requires a clean Core checkout at origin/main');
  if (cp.execFileSync('git', ['-C', core, 'status', '--porcelain'], { encoding: 'utf8' }).trim()) throw new Error('fallback-record requires an unchanged Core checkout');
  const source = (file) => cp.execFileSync('git', ['-C', core, 'show', `${coreRef}:${file}`], { encoding: 'utf8' });
  const index = source('server/index.cjs');
  const load = (file) => {
    const Module = require('node:module');
    const m = new Module(path.join(core, file), module);
    m.filename = path.join(core, file); m.paths = Module._nodeModulePaths(path.dirname(m.filename));
    m._compile(source(file), m.filename); return m.exports;
  };
  const { json, unauthorized } = load('server/http.cjs');
  const modelRoutes = load('server/routes/models.cjs').createModelRoutes({ json, env: {}, modelManager: { enabled: false }, rerankSettings: new Proxy({}, { get() { throw new Error('unsupported method accessed settings'); } }), service: { modelScanCache: { clear() {} } }, currentWorkspace: () => ({}) });
  const authRoutes = load('server/routes/auth.cjs').createAuthRoutes({ json, readJson: async () => ({}) });
  const providerRoutes = load('server/routes/providers.cjs').createProviderRoutes({ json });
  const serveStatic = load('server/spa-routes.cjs').createStaticFallback({ json, staticFiles: { resolve: () => null } });
  const doc = JSON.parse(fs.readFileSync(fixture));
  const routes = rustRoutes(toml);
  const credentials = load('server/rust-mcp-credentials.cjs');
  const registry = load('server/rust-providers.cjs');
  const providerModels = load('server/rust-provider-models.cjs');
  const broker = load('server/rust-chatgpt-oauth.cjs');
  const selected = (c) => {
    const p = c.path, m = c.method;
    if (c.shape === 'dotted' && p.startsWith('/api/providers/') && ['/api/', '/api/models'].includes(new URL(p, ORIGIN).pathname)) return true;
    if (p.startsWith('/api/models/') && p.endsWith('/') && routes.some((r) => !r.path.includes('{') && p.replace(/\/+$/, '') === r.path)) return true;
    if (['/api/models/installed', '/api/models/downloads', '/api/models/capabilities', '/api/models/hardware', '/api/models/estimate', '/api/models/preset/suggest'].includes(p)) return m !== 'GET';
    if (p === '/api/models/assignments') return m !== 'GET';
    if (p === '/api/models/prune-missing') return m !== 'POST';
    if (p === '/api/models/reranker') return !['GET', 'PUT', 'DELETE'].includes(m);
    if (['/api/mcp-keys', '/api/mcp-oauth'].some((b) => p === b || p.startsWith(`${b}/`))) return !credentials.owned(m, p) || (p === '/api/mcp-keys/servers' && ['PUT', 'DELETE'].includes(m)) || (['/api/mcp-oauth/servers', '/api/mcp-oauth/callback'].includes(p) && m === 'DELETE');
    if (p === '/api/providers' || p.startsWith('/api/providers/')) return !(registry.owned(m, p) || providerModels.owned(p) || broker.owned(m,p) || (p === '/api/providers/chatgpt' && m === 'GET') || (p === '/api/providers/test' && m === 'POST'));
    if ((p === '/api/providers/test' && ['PUT', 'DELETE'].includes(m)) || (p === '/api/providers/chatgpt' && m === 'DELETE')) return false;
    if ((p === '/api/integrations/storage/files' || p.startsWith('/api/integrations/storage/files/')) && m !== 'GET') return true;
    if (!['/api/connectors', '/api/integrations/storage', '/api/plugins/directory', '/api/toolboxes', '/api/admin/offsite-backup', '/api/providers'].some((b) => p === b || p.startsWith(`${b}/`))) return false;
    const r = routes.find((r) => !r.path.includes('{') && (p === r.path || p === `${r.path}/`));
    return !!r && (p !== r.path || (r.methods.length && !r.methods.includes(m)));
  };
  const rows = [];
  const extras = ['/api/models/prune-missing', '/api/models/reranker', '/api/models/pull', '/api/models/delete', '/api/models/load', '/api/models/unload', '/api/models/grid-unmatched', '/api/models/grid-unmatched/', '/api/models/pull/', '/api/models/grid-unmatched/deep', '/api/connectors/grid-unmatched/deep', '/api/integrations/storage/grid-unmatched', '/api/toolboxes/grid-unmatched', '/api/providers/grid-unmatched/deep', '/api/providers/chatgpt/grid-unmatched'].flatMap((p) => METHODS.filter((m) => p === '/api/models/reranker' ? !['GET', 'PUT', 'DELETE'].includes(m) : m !== 'POST' || !['/api/models/prune-missing', '/api/models/pull', '/api/models/delete', '/api/models/load', '/api/models/unload'].includes(p)).flatMap((method) => ROLES.filter((who) => !who.endsWith('-nocsrf') || UNSAFE.includes(method)).map((who) => ({ id: `${method} ${p} ${who}`, family: familyOf(p), route: p, shape: 'unmatched', method, path: p, who }))));
  for (const c of [...doc.cases.filter(selected), ...extras]) {
    const authn = c.who === 'anon' ? null : { user: { id: 'synthetic-grid-user', role: c.who.startsWith('admin') ? 'admin' : 'member' } };
    const noop = async () => false;
    const ctx = { json, unauthorized, providerRoutes, modelRoutes, serveStatic,
      parseRequestUrl: (req) => new URL(req.url, ORIGIN), publicAuthRoutes: new Set(),
      requestAuth: { authenticate: () => authn, csrfValid: () => !c.who.endsWith('-nocsrf'), browserOnly: () => false },
      authService: { originValid: () => true }, changeJournal: { touch() {} },
      vaultMirrorLib: { mirrorTrigger: () => null }, publicHeaders: () => {},
      require: (id) => id === './project-store-client.cjs' ? {} : { refuseOwned: () => false, refuseOwnedRoute: () => false },
      errorResponse: (e) => { throw e; }, console,
    };
    for (const n of ['webAddressRoutes', 'featureRoutes', 'chatFramingRoutes', 'exportRoutes', 'importRoutes', 'accountRoutes', 'codeRoutes', 'browserRoutes', 'storageRoutes', 'chatListRoutes', 'usageRoutes', 'reasoningSettingsRoutes', 'routingModeRoutes', 'samplingSettingsRoutes', 'embeddingRoutes', 'inferenceMachineRoutes', 'chatRoutes']) ctx[n] = noop;
    ctx.authRoutes = authRoutes; ctx.deviceRoutes = { open: noop }; ctx.readyRoutes = noop;
    const publicStart = index.indexOf('function publicHeaders(res)');
    const publicEnd = index.indexOf('\n}', publicStart) + 2;
    const start = index.indexOf('async function handleRequestScoped(req, res)');
    const end = index.indexOf('\nasync function handleRequest(req,res)', start);
    vm.createContext(ctx);
    vm.runInContext(index.slice(publicStart, publicEnd) + '\n' + index.slice(start, end), ctx);
    const headers = { 'x-noevia-api': '1' };
    let status, text;
    const res = { setHeader: (k,v) => { headers[k.toLowerCase()] = v; }, writeHead: (s,h) => { status=s; for (const [k,v] of Object.entries(h)) headers[k.toLowerCase()]=v; }, end: (b) => { text = c.method === 'HEAD' ? '' : b; } };
    const req = { method: c.method, url: c.path, headers: { host: HOST } };
    ctx.publicHeaders(res);
    if (!credentials.refuseOwned(req, res, { parseRequestUrl: ctx.parseRequestUrl, json })) await ctx.handleRequestScoped(req, res);
    if (c.status !== undefined && (status !== c.status || JSON.stringify(parseBody(text)) !== JSON.stringify(c.body))) throw new Error(`scoped oracle differs from stack: ${c.id}: ${status} ${text}`);
    rows.push({ ...c, status, body: parseBody(text), wireBody: text, headers, answeredBy: 'front' });
  }
  writeFixture(out, { coreRef, note: 'Scoped Node handler at origin/main, real residual mounts, synthetic identity and disabled engine. Existing full-stack grid supplies the comparison baseline. Identity backend and runtime facts are tested separately by Rust replay.', cases: rows });
}

const [mode, ...args] = process.argv.slice(2);
const modes = { 'fallback-record': fallbackRecord, run, merge: async (...a) => merge(...a), classify: async (...a) => classify(...a), selftest: async () => selftest() };
(modes[mode] ? modes[mode](...args) : Promise.reject(new Error('usage: run|merge|classify|selftest')))
  .catch((e) => { console.error(e.stack || e.message); process.exitCode = 1; });
