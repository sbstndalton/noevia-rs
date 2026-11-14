#!/usr/bin/env node
'use strict';
// Recorded PUT /api/routing-default (track R12) for crates/server-routing-default `put` and the front's route.
//
//   NOEVIA_CORE_CHECKOUT=<noevia-core checkout> \
//     node crates/server-routing-default/tests/record_put.cjs > crates/server-routing-default/tests/fixtures/node-routing-default-put.v1.json
//   ... node crates/server-routing-default/tests/record_put.cjs --check crates/server-routing-default/tests/fixtures/node-routing-default-put.v1.json
//     records again and fails unless the result is byte-identical to the committed file.
//
// Every expectation comes from noevia-core's own code: the real models.cjs route (createModelRoutes, PUT
// /api/routing-default) over a real workspace store (workspace.cjs createWorkspaceStore) in a fresh temporary data
// directory per case, with the account's projects.json saved through a stand-in for the front's project store that
// keeps the text Node sends (rust-projects.cjs saveRemote). Date.now answers a fixed time. Recorded: the reply
// (status and body), preferences.json after the request, and the projects.json text Node saved (null: no save).
// Inputs are synthetic and QA-named. Cases marked `difference` are refused by the port on purpose (put.rs header).
// Each source file's sha256 is recorded, so a changed Node file needs a fresh recording. stdout is the fixture.

const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { Readable } = require('node:stream');

const checkout = process.env.NOEVIA_CORE_CHECKOUT;
if (!checkout) {
  process.stderr.write('Set NOEVIA_CORE_CHECKOUT to a noevia-core checkout.\n');
  process.exit(2);
}
const root = path.resolve(checkout);
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = (...a) => process.stderr.write(`${a.map(String).join(' ')}\n`);
const READ_FILES = ['server/routes/models.cjs', 'server/workspace.cjs', 'server/rust-projects.cjs', 'server/http.cjs'];
const sha = (f) => crypto.createHash('sha256').update(fs.readFileSync(path.join(root, f))).digest('hex');

const NOW = 1763078400000;
Date.now = () => NOW;
const USER = 'aaaaaaaa-1111-4aaa-8aaa-aaaaaaaaaaaa';
const { createModelRoutes } = require(path.join(root, 'server/routes/models.cjs'));
const { createWorkspaceStore } = require(path.join(root, 'server/workspace.cjs'));
const { readBody, readJson } = require(path.join(root, 'server/http.cjs'));

const hex = (b) => Buffer.from(b).toString('hex');
const text = (t) => ({ text: t });
const project = (id, extra = '') => `{"id":"${id}","name":"QA ${id}","modes":["chat"]${extra}}`;
const projectsOf = (...items) => text(`{"projects":[${items.join(',')}]}`);
const DEFAULT_PREFS = text('{"qaNote":"keep"}');
const DEFAULT_PROJECTS = projectsOf(project('proj-qa-1', ',"routing":"auto"'), project('proj-qa-2', ',"routing":"manual"'), project('proj-qa-3'));
const big = (n, json) => json + ' '.repeat(n - Buffer.byteLength(json));
const deep = (n) => '['.repeat(n) + ']'.repeat(n);

// [name, body (text|hex), preferences (text|hex|null = no file), projects (text|null = no file), difference?]
const cases = [
  // The body.
  ['manual', text('{"routing":"manual"}')],
  ['auto', text('{"routing":"auto"}')],
  ['manual-apply', text('{"routing":"manual","applyToExisting":true}')],
  ['auto-apply', text('{"routing":"auto","applyToExisting":true}')],
  ['apply-string-true', text('{"routing":"manual","applyToExisting":"true"}')],
  ['apply-one', text('{"routing":"manual","applyToExisting":1}')],
  ['apply-null', text('{"routing":"manual","applyToExisting":null}')],
  ['apply-false', text('{"routing":"manual","applyToExisting":false}')],
  ['routing-case', text('{"routing":"Manual"}')],
  ['routing-space', text('{"routing":"manual "}')],
  ['routing-number', text('{"routing":1}')],
  ['routing-missing', text('{}')],
  ['routing-escaped', text('{"routing":"\\u006danual","applyToExisting":true}')],
  ['routing-duplicate', text('{"routing":"bad","routing":"manual"}')],
  ['routing-duplicate-bad-last', text('{"routing":"manual","routing":"bad"}')],
  ['body-null', text('null')],
  ['body-array', text('["manual"]')],
  ['body-string', text('"manual"')],
  ['body-number', text('0')],
  ['body-empty', text('')],
  ['body-malformed', text('{')],
  ['body-trailing-comma', text('{"routing":"manual",}')],
  ['body-bom', { hex: hex(Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), Buffer.from('{"routing":"manual"}')])) }],
  ['body-whitespace', text('\n\t {"routing" : "manual" }\r\n')],
  ['body-invalid-utf8-sibling', { hex: hex(Buffer.concat([Buffer.from('{"qa":"'), Buffer.from([0xff, 0xc3]), Buffer.from('","routing":"manual"}')])) }],
  ['body-lone-surrogate-sibling', text('{"qa":"\\ud800","routing":"manual"}')],
  ['body-at-limit', text(big(1024 * 1024, '{"routing":"manual"}'))],
  ['body-over-limit', text(big(1024 * 1024 + 1, '{"routing":"manual"}'))],
  ['body-huge-number-sibling', text('{"qa":1e999,"routing":"manual"}'), undefined, undefined, 'unholdable-body'],
  // preferences.json as stored.
  ['prefs-missing', text('{"routing":"manual"}'), null],
  ['prefs-empty-object', text('{"routing":"manual"}'), text('{}')],
  ['prefs-keeps-order', text('{"routing":"auto"}'), text('{"a":1,"defaultRouting":"manual","z":[1,{"b":null}]}')],
  ['prefs-index-keys', text('{"routing":"manual"}'), text('{"b":1,"2":"x","a":2,"1":"y","10":"z"}')],
  ['prefs-array', text('{"routing":"manual"}'), text('[1,{"a":2},"three"]')],
  ['prefs-string', text('{"routing":"manual"}'), text('"q😀a"')],
  ['prefs-null', text('{"routing":"manual"}'), text('null')],
  ['prefs-zero', text('{"routing":"manual"}'), text('0')],
  ['prefs-false', text('{"routing":"manual"}'), text('false')],
  ['prefs-true', text('{"routing":"manual"}'), text('true')],
  ['prefs-number', text('{"routing":"manual"}'), text('5')],
  ['prefs-empty-string', text('{"routing":"manual"}'), text('""')],
  ['prefs-empty-file', text('{"routing":"manual"}'), text('')],
  ['prefs-malformed', text('{"routing":"manual"}'), text('{"a":')],
  ['prefs-bom', text('{"routing":"manual"}'), { hex: hex(Buffer.concat([Buffer.from([0xef, 0xbb, 0xbf]), Buffer.from('{"a":1}')])) }],
  ['prefs-invalid-utf8', text('{"routing":"manual"}'), { hex: hex(Buffer.concat([Buffer.from('{"note":"'), Buffer.from([0xff]), Buffer.from('"}')])) }],
  ['prefs-lone-surrogate', text('{"routing":"manual"}'), text('{"x":"\\ud83d","y":"\\ude00x"}')],
  ['prefs-unicode-and-escapes', text('{"routing":"auto"}'), text('{"qa":"é\\u0000\\n\\"\\\\/ \\u2028","n":-0,"f":1.5e-7,"big":12345678901234567890}')],
  ['prefs-duplicate-keys', text('{"routing":"auto"}'), text('{"defaultRouting":"manual","q":1,"defaultRouting":"x"}')],
  ['prefs-nested-deep-ok', text('{"routing":"manual"}'), text(`{"d":${deep(100)}}`)],
  ['prefs-huge-number', text('{"routing":"manual"}'), text('{"x":1e999}'), undefined, 'unholdable-preferences'],
  ['prefs-nested-too-deep', text('{"routing":"manual"}'), text(`{"d":${deep(200)}}`), undefined, 'unholdable-preferences'],
  // projects.json as stored, with applyToExisting.
  ['projects-missing', text('{"routing":"manual","applyToExisting":true}'), undefined, null],
  ['projects-empty', text('{"routing":"manual","applyToExisting":true}'), undefined, projectsOf()],
  ['projects-key-null', text('{"routing":"manual","applyToExisting":true}'), undefined, text('{"projects":null}')],
  ['projects-no-key', text('{"routing":"manual","applyToExisting":true}'), undefined, text('{}')],
  ['projects-malformed', text('{"routing":"manual","applyToExisting":true}'), undefined, text('{"projects":[')],
  ['projects-all-same', text('{"routing":"auto","applyToExisting":true}'), undefined, projectsOf(project('proj-qa-1', ',"routing":"auto"'), project('proj-qa-2', ',"routing":"auto"'))],
  ['projects-odd-routing', text('{"routing":"auto","applyToExisting":true}'), undefined, projectsOf(project('proj-qa-1', ',"routing":1'), project('proj-qa-2', ',"routing":null'), project('proj-qa-3', ',"routing":"Auto"'))],
  ['projects-updated-at-in-place', text('{"routing":"manual","applyToExisting":true}'), undefined, projectsOf(`{"updatedAt":1,"id":"proj-qa-1","routing":"auto","modes":["chat"],"name":"QA one"}`)],
  ['projects-internal-ids', text('{"routing":"manual","applyToExisting":true}'), undefined, projectsOf(project('cowork-diary-extras'), project('cowork-chat-context-qa'), project('specialist-qa'), project('proj-qa-4'))],
  ['projects-null-files-dropped', text('{"routing":"manual","applyToExisting":true}'), undefined, projectsOf(project('proj-qa-1', ',"files":[null,{"name":"qa.txt","type":"text/plain","size":1,"textRef":"' + 'a'.repeat(64) + '"},3]'))],
  ['projects-lone-surrogate-name', text('{"routing":"manual","applyToExisting":true}'), undefined, projectsOf(`{"id":"proj-qa-1","name":"QA \\ud83d","modes":["chat"]}`)],
  ['projects-index-keys', text('{"routing":"manual","applyToExisting":true}'), undefined, projectsOf(`{"id":"proj-qa-1","2":"b","1":"a","modes":["chat"]}`)],
  ['projects-not-applied', text('{"routing":"manual"}'), undefined, DEFAULT_PROJECTS],
];

function request(body) {
  const bytes = body.hex !== undefined ? Buffer.from(body.hex, 'hex') : Buffer.from(body.text, 'utf8');
  // Chunked as a socket delivers it, so readBody's limit counts as it does there.
  const chunks = [];
  for (let i = 0; i < bytes.length; i += 65536) chunks.push(bytes.subarray(i, i + 65536));
  const req = Readable.from(chunks);
  req.method = 'PUT';
  req.headers = { 'content-type': 'application/json' };
  return req;
}

async function record([name, body, prefs = DEFAULT_PREFS, projects = DEFAULT_PROJECTS, difference]) {
  const data = fs.mkdtempSync(path.join(os.tmpdir(), 'noevia-qa-routing-put-'));
  const dir = path.join(data, 'users', USER);
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  const write = (file, v) => fs.writeFileSync(path.join(dir, file), v.hex !== undefined ? Buffer.from(v.hex, 'hex') : v.text);
  if (prefs) write('preferences.json', prefs);
  if (projects) write('projects.json', projects);
  let saved = null;
  let version = 0;
  const projectStore = {
    async read(user, file) {
      const p = path.join(data, 'users', user, file);
      return { version: `v${version}`, text: fs.existsSync(p) ? fs.readFileSync(p, 'utf8') : null };
    },
    async write(user, file, v, t) {
      if (file === 'projects.json') saved = t;
      fs.writeFileSync(path.join(data, 'users', user, file), t);
      version += 1;
      return { ok: true, version: `v${version}` };
    },
  };
  const store = createWorkspaceStore(data, { id: 'default', label: 'QA local', baseUrl: 'http://127.0.0.1:1', apiKey: '', shared: true }, null, { projectStore });
  const routes = createModelRoutes({ json: (res, status, b) => { res.status = status; res.body = JSON.stringify(b); return true; }, readBody, readJson, fetchJson: async () => ({ ok: false }), env: {}, modelManager: {}, currentWorkspace: () => store.get(USER), service: {} });
  const res = {};
  let threw = null;
  try {
    await routes(request(body), res, { path: '/api/routing-default', authn: { user: { id: USER, role: 'member' } }, url: new URL('http://h/api/routing-default') });
    const ws = store.get(USER);
    if (ws.pendingStoreWrites()) await ws.pendingStoreWrites();
  } catch (e) { threw = String(e && e.message || e); }
  const after = path.join(dir, 'preferences.json');
  const out = {
    name,
    body: body.text !== undefined && Buffer.byteLength(body.text) > 4096 ? { repeatSpacesTo: Buffer.byteLength(body.text), text: body.text.trimEnd() } : body,
    preferences: prefs, projects,
    ...(difference ? { difference } : {}),
    expected: {
      status: res.status ?? null, reply: res.body ?? null, threw,
      preferences: fs.existsSync(after) ? fs.readFileSync(after).toString('hex') : null,
      projectsSaved: saved,
    },
  };
  fs.rmSync(data, { recursive: true, force: true });
  return out;
}

(async () => {
  const results = [];
  for (const c of cases) results.push(await record(c));
  const fixture = JSON.stringify({
    generator: 'crates/server-routing-default/tests/record_put.cjs',
    user: USER, now: NOW,
    sources: Object.fromEntries(READ_FILES.map((f) => [f, sha(f)])),
    cases: results,
  }, null, 1) + '\n';
  const check = process.argv.indexOf('--check');
  if (check !== -1) {
    const committed = fs.readFileSync(process.argv[check + 1], 'utf8');
    if (committed !== fixture) { process.stderr.write('routing-default PUT fixture differs from a fresh recording\n'); process.exit(1); }
    process.stderr.write(`routing-default PUT fixture matches (${results.length} cases)\n`);
    return;
  }
  process.stdout.write(fixture);
})().catch((e) => { process.stderr.write(`${e.stack || e}\n`); process.exit(1); });
