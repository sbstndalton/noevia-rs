'use strict';
// Run against the pre-move Core checkout (fixtures/routing-default-put-oracle.core-ref), never a reimplementation:
// the actual models route (PUT /api/routing-default), http.cjs readBody, and the actual workspace store over the
// project-store stand-in, on real files. Prints the cases as JSON: node routing-default-put-oracle.cjs > out.json
const fs = require('node:fs'), os = require('node:os'), path = require('node:path');
const { Readable } = require('node:stream');
const { EventEmitter } = require('node:events');
const CORE = process.env.CORE_ROOT;
const { createModelRoutes } = require(CORE + '/server/routes/models.cjs');
const { createWorkspaceStore } = require(CORE + '/server/workspace.cjs');
const { createStubStore } = require(CORE + '/tests/server/project-store-stub.cjs');
const http = require(CORE + '/server/http.cjs');
const NOW = 1800000000000;
const USER = '11111111-2222-4333-8444-555555555555';
const realNow = Date.now;
const proj = (id, routing, extra = {}) => ({ id, name: id, goal: '', instructions: '', files: [], modes: ['chat'], chats: [], createdAt: 5, updatedAt: 6, ...(routing === undefined ? {} : { routing }), ...extra });
const MIXED = [proj('a', 'manual'), proj('b', 'auto'), proj('c'), proj('d', 5), proj('cowork-diary-extras', 'manual'), proj('e', 'manual', { updatedAt: 1, note: 'kept' })];
const S = (v) => JSON.stringify(v, null, 2);
// DELIBERATE DEVIATION from the real route, sbstndalton/noevia#2388 (item 1): Node's applyToExisting also switched, stamped and
// counted the internal projects (the Diary extras project and the cowork-chat-context-* shadow projects), which changes
// them from a user-facing default. The native handler leaves them alone and out of `updated`. `expected` below is Node's
// answer corrected that way (the real route's own output, with the internal projects restored to their initial value and
// taken out of the count); `node` keeps what the real route did.
const internal = (p) => p && (p.id === 'cowork-diary-extras' || String(p.id).startsWith('cowork-chat-context-'));
function corrected(reply, initialProjects, finalText) {
  if (!initialProjects || !reply || reply.status !== 200 || finalText === null) return null;
  const before = JSON.parse(initialProjects).projects, after = JSON.parse(finalText).projects;
  let changedInternal = 0;
  const fixed = after.map((p, i) => {
    if (!internal(p)) return p;
    if (JSON.stringify(p) !== JSON.stringify(before[i])) changedInternal++;
    return before[i];
  });
  if (!changedInternal) return null;
  const updated = reply.body.updated - changedInternal;
  return { reply: { status: 200, body: { ...reply.body, updated } }, projects: updated > 0 ? S({ projects: fixed }) : initialProjects };
}
(async () => {
  const cases = [];
  // [name, prefs (raw text or null = no file), projects (list or null = no file), body (string | {hex}), role?]
  const table = [
    ['manual-new-file', null, [], '{"routing":"manual"}'],
    ['auto-new-file', null, [], '{"routing":"auto"}'],
    ['keeps-other-keys', '{"theme":"dark","defaultRouting":"auto","z":[1,2]}', [], '{"routing":"manual"}'],
    ['appends-default-last', '{"theme":"dark"}', [], '{"routing":"auto"}'],
    ['integer-keys-first', '{"b":1,"2":3,"a":2,"10":4}', [], '{"routing":"manual"}'],
    ['duplicate-keys-in-file', '{"x":1,"defaultRouting":"auto","x":2}', [], '{"routing":"manual"}'],
    ['file-null', 'null', [], '{"routing":"manual"}'],
    ['file-zero', '0', [], '{"routing":"manual"}'],
    ['file-true', 'true', [], '{"routing":"manual"}'],
    ['file-number', '42', [], '{"routing":"manual"}'],
    ['file-empty-string', '""', [], '{"routing":"manual"}'],
    ['file-array', '[1,"two",{"three":3}]', [], '{"routing":"manual"}'],
    ['file-string', '"ab"', [], '{"routing":"manual"}'],
    ['file-invalid-json', '{ not json', [], '{"routing":"manual"}'],
    ['file-empty', '', [], '{"routing":"manual"}'],
    ['file-unicode', '{"n":"caf\\u00e9 \\ud83d\\ude00"}', [], '{"routing":"auto"}'],
    ['apply-mixed-auto', null, MIXED, '{"routing":"auto","applyToExisting":true}'],
    ['apply-mixed-manual', null, MIXED, '{"routing":"manual","applyToExisting":true}'],
    ['apply-none-differ', null, [proj('a', 'auto'), proj('b', 'auto')], '{"routing":"auto","applyToExisting":true}'],
    ['apply-no-projects', null, [], '{"routing":"auto","applyToExisting":true}'],
    ['apply-missing-projects-file', null, null, '{"routing":"manual","applyToExisting":true}'],
    ['apply-false', null, MIXED, '{"routing":"auto","applyToExisting":false}'],
    ['apply-string-true', null, MIXED, '{"routing":"auto","applyToExisting":"true"}'],
    ['apply-one', null, MIXED, '{"routing":"auto","applyToExisting":1}'],
    ['apply-null', null, MIXED, '{"routing":"auto","applyToExisting":null}'],
    ['apply-duplicate-last-true', null, MIXED, '{"applyToExisting":false,"routing":"auto","applyToExisting":true}'],
    ['apply-duplicate-last-false', null, MIXED, '{"applyToExisting":true,"routing":"auto","applyToExisting":false}'],
    ['default-alone-leaves-projects', null, MIXED, '{"routing":"manual"}'],
    ['apply-internal-only', null, [proj('cowork-diary-extras', 'manual'), proj('cowork-chat-context-abc', 'manual')], '{"routing":"auto","applyToExisting":true}'],
    ['apply-internal-lookalikes', null, [proj('cowork-diary-extras-2', 'manual'), proj('cowork-chat-contextual', 'manual'), proj('cowork-chat-context-', 'manual'), proj('my-cowork-diary-extras', 'manual')], '{"routing":"auto","applyToExisting":true}'],
    ['apply-internal-and-user', null, [proj('a', 'manual'), proj('cowork-chat-context-x', 'manual'), proj('b', 'auto'), proj('cowork-diary-extras')], '{"routing":"auto","applyToExisting":true}'],
    ['empty-body', null, [], ''],
    ['whitespace-body', null, [], '  \n'],
    ['truncated', null, [], '{'],
    ['trailing-comma', null, [], '{"routing":"auto",}'],
    ['trailing-garbage', null, [], '{"routing":"auto"} x'],
    ['bom', null, [], '﻿{"routing":"auto"}'],
    ['null-body', null, [], 'null'],
    ['array-body', null, [], '[{"routing":"auto"}]'],
    ['string-body', null, [], '"auto"'],
    ['number-body', null, [], '5'],
    ['true-body', null, [], 'true'],
    ['empty-object', null, [], '{}'],
    ['routing-fast', null, MIXED, '{"routing":"fast","applyToExisting":true}'],
    ['routing-upper', null, [], '{"routing":"AUTO"}'],
    ['routing-padded', null, [], '{"routing":" auto"}'],
    ['routing-null', null, [], '{"routing":null}'],
    ['routing-number', null, [], '{"routing":1}'],
    ['routing-array', null, [], '{"routing":["auto"]}'],
    ['routing-object', null, [], '{"routing":{"auto":true}}'],
    ['routing-duplicate-last-wins', null, [], '{"routing":"fast","routing":"manual"}'],
    ['routing-escaped-key', null, [], '{"\\u0072outing":"manual"}'],
    ['routing-escaped-value', null, [], '{"routing":"au\\u0074o"}'],
    ['overflow-ignored', null, [], '{"x":1e999,"routing":"auto"}'],
    ['lone-surrogate-ignored', null, [], '{"x":"\\ud800","routing":"auto"}'],
    ['reserved-plane-ignored', null, [], JSON.stringify({ x: String.fromCodePoint(0x10f800), routing: 'auto' })],
    ['deep-ignored', null, [], '{"x":' + '['.repeat(1000) + '0' + ']'.repeat(1000) + ',"routing":"auto"}'],
    ['invalid-utf8-ignored', null, [], { hex: Buffer.from('{"x":"', 'latin1').toString('hex') + 'ff' + Buffer.from('","routing":"auto"}', 'latin1').toString('hex') }],
    ['big-valid-body', null, [], { pad: 1024 * 1024 - 64, routing: 'manual' }],
    ['oversize-body', null, [], { pad: 1024 * 1024 + 1, routing: 'manual' }],
  ];
  for (const [name, prefs, projects, body] of table) {
    const root = fs.mkdtempSync(path.join(os.tmpdir(), 'synthetic-routing-'));
    const dir = path.join(root, 'users', USER);
    fs.mkdirSync(dir, { recursive: true });
    if (prefs !== null) fs.writeFileSync(path.join(dir, 'preferences.json'), prefs);
    if (projects !== null) fs.writeFileSync(path.join(dir, 'projects.json'), S({ projects }));
    const initialProjects = projects === null ? null : S({ projects });
    Date.now = () => NOW;
    const sent = [];
    try {
      const store = createWorkspaceStore(root, { id: 'local', label: 'Local', baseUrl: '', apiKey: '' }, null, { projectStore: createStubStore(root), onStoreSave: (p) => saves.push(p) });
      var saves = [];
      const ws = store.get(USER);
      const routes = createModelRoutes({
        json: (res, status, b) => { sent.push({ status, body: b }); },
        readBody: http.readBody, readJson: http.readJson,
        fetchJson: async () => ({ ok: false }), env: {}, modelManager: { enabled: false, kind: 'llamacpp', capabilities: {} },
        currentWorkspace: () => ws,
        service: { modelScanCache: new Map(), invalidateModelReads() {} },
      });
      const bytes = body && body.hex !== undefined ? Buffer.from(body.hex, 'hex')
        : body && body.pad !== undefined ? Buffer.from(JSON.stringify({ pad: 'x'.repeat(body.pad), routing: body.routing }), 'utf8') : Buffer.from(body, 'utf8');
      const req = Readable.from(bytes.length ? [bytes] : []);
      req.method = 'PUT';
      const res = Object.assign(new EventEmitter(), { setHeader() {} });
      await routes(req, res, { path: '/api/routing-default', authn: { user: { id: USER, role: 'member' } }, url: new URL('http://localhost/api/routing-default') });
      await Promise.all(saves);
    } catch (e) { sent.push({ thrown: String(e && e.message || e) }); }
    const read = (f) => { try { return fs.readFileSync(path.join(dir, f), 'utf8'); } catch { return null; } };
    const node = { reply: sent[0], preferences: read('preferences.json'), projects: read('projects.json') };
    const fix = corrected(node.reply, initialProjects, node.projects);
    cases.push({ name, initial: { preferences: prefs, projects: initialProjects }, body: body && body.hex !== undefined ? { hex: body.hex } : body && body.pad !== undefined ? { pad: body.pad, routing: body.routing } : { text: body },
      expected: fix ? { ...node, ...fix, deviation: '#2388' } : node, ...(fix ? { node } : {}) });
    fs.rmSync(root, { recursive: true, force: true });
  }
  Date.now = realNow;
  process.stdout.write(JSON.stringify({ core: process.env.CORE_REF || '', node: process.version, now: NOW, user: USER, cases }, null, 1));
})().catch((e) => { console.error(e); process.exitCode = 1; });
