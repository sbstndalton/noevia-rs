#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own projects.cjs `writeProjectTextFile` (over workspace.cjs,
// uploads.cjs ingest, project-folders.cjs, project-edit-target.cjs storageAccount and the dav-parse.wasm
// core pins) on synthetic, QA-named projects against a scripted storage, and records per case: the
// answer (the file recorded, or the error text), the storage calls in order, the project as saved and
// the index work asked for. Track R14 step 5: crates/server-projects/src/text_write.rs replays every
// case with the same scripted storage (its tests).
//
//   DAV_PARSE_WASM=<dav-parse.wasm whose sha256 is core's server/dav-parse.lock pin> \
//     node tools/gen-text-write-fixtures.cjs <noevia-core checkout> <output.json>
//   ... node tools/gen-text-write-fixtures.cjs <noevia-core checkout> --check <fixture.json>
//
// The scripted storage (both sides): `files` path -> { text, etag } (etag '' = the server reports
// none), `dirs`, and `fail.{version,read,mkdir,write}` path -> { status, message, code?, upstream? }.
// fileVersion: the failure, else { exists, etag }; readBinaryFile: the failure, the bytes, or a 404;
// createFolder: the failure, else { existed } (and the folder exists from then on); writeFile: the
// failure, a 412 `changed` when If-Match differs from the file's ETag or If-None-Match meets a file,
// else the file is stored with ETag `"qa-etag-<n>"` and { etag, created }; discardWritten removes a
// file whose ETag matches. Date.now is fixed. Each core file loaded is listed with its sha256.
'use strict';
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const core = path.resolve(process.argv[2] || '');
const checkAt = process.argv[3] === '--check' ? process.argv[4] : null;
const out = checkAt ? null : process.argv[3];
if (!process.argv[2] || (!out && !checkAt)) throw Error('usage: gen-text-write-fixtures.cjs <core> <output.json> | --check <fixture.json>');
if (!process.env.DAV_PARSE_WASM) throw Error('DAV_PARSE_WASM must name the pinned dav-parse.wasm');
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = () => {};
const NOW = 1700000000000;
Date.now = () => NOW;
const req = (m) => require(path.join(core, 'server', m));
const { createWorkspaceStore } = req('workspace.cjs');
const { createProjectStore } = req('projects.cjs');
const { createProjectFolder } = req('project-folders.cjs');
const edits = req('project-edit-target.cjs');

const USER = 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee';
const ROOT = 'noevia projects';
const pretty = (v) => JSON.stringify(v, null, 2);
const sha = (b) => crypto.createHash('sha256').update(b).digest('hex');
const ref = (t) => sha(Buffer.from(t, 'utf8'));

// ── Synthetic projects and storage ──────────────────────────────────────────
const F = `${ROOT}/QA Alpha`;
const NOTE = 'QA note: stored text.';
const PLAIN = 'QA plain local text.';
const DAV = { kind: 'webdav', baseUrl: 'https://qa-dav.invalid/remote.php/dav/files/qa', bucket: '', region: 'us-east-1', username: 'qa-dav' };
const OTHER_DAV = { ...DAV, username: 'qa-other' };
const account = (c) => edits.storageAccount(c);
const alpha = (mut = () => {}) => {
  const p = {
    id: 'qa-alpha', name: 'QA Alpha', modes: ['chat'], chats: [], instructionSkills: {}, projectFolder: F, sourceFolders: [F],
    files: [
      { name: `${F}/Text/notes.md`, source: F, textRef: ref(NOTE), chars: NOTE.length, attachment: { id: sha(Buffer.from(NOTE)), bytes: NOTE.length, group: 'Text', state: 'ready' } },
      { name: 'plain.txt', textRef: ref(PLAIN), chars: PLAIN.length },
      { name: `${F}/Text/partial.md`, source: F, content: 'QA partial', attachment: { id: sha(Buffer.from('QA partial')), bytes: 10, group: 'Text', state: 'partial' } },
    ],
  };
  mut(p);
  return p;
};
const bare = (mut = () => {}) => { const p = { id: 'qa-bare', name: 'QA Bare', modes: ['chat'], chats: [], instructionSkills: {}, files: [{ name: 'plain.txt', textRef: ref(PLAIN), chars: PLAIN.length }] }; mut(p); return p; };
const TEXTS = { 'qa-alpha': [NOTE, PLAIN], 'qa-bare': [PLAIN] };
const stored = (extra = {}) => ({ files: { [`${F}/Text/notes.md`]: { text: NOTE, etag: '"qa-etag-a"' } }, dirs: [ROOT, F, `${F}/Text`], fail: {}, ...extra });
const editNotes = (text, extra = {}) => ({ project: 'qa-alpha', name: 'notes.md', text, options: { expectName: `${F}/Text/notes.md`, expectContent: NOTE, expectAttachment: sha(Buffer.from(NOTE)), ...extra } });
const adoptPlain = (project, extra = {}) => ({ project, name: 'plain.txt', text: `${PLAIN} more`, options: { expectName: 'plain.txt', expectContent: PLAIN, expectAttachment: null, adoptTo: `${project === 'qa-alpha' ? F : `${ROOT}/qa-bare`}/Text/plain.txt`, adoptAccount: account(DAV), ...extra } });
const err = (status, message, more = {}) => ({ status, message, ...more });

const cases = [];
const add = (name, projects, conn, storage, call) => cases.push({ name, projects: pretty({ projects }), connection: conn, storage, call });
// Creates.
add('create-local', [bare()], null, null, { project: 'qa-bare', name: 'new.md', text: 'QA new text', options: {} });
add('create-remote', [alpha()], DAV, stored(), { project: 'qa-alpha', name: 'new.md', text: 'QA new text', options: {} });
add('create-remote-no-folder', [bare()], DAV, { files: {}, dirs: [], fail: {} }, { project: 'qa-bare', name: 'new.md', text: 'QA new text', options: {} });
add('create-remote-reserved', [bare((p) => { p.reservedFolder = `${ROOT}/QA Reserved`; })], DAV, { files: {}, dirs: [ROOT], fail: {} }, { project: 'qa-bare', name: 'new.md', text: 'QA', options: {} });
add('create-remote-write-fails', [alpha()], DAV, stored({ fail: { write: { [`${F}/Text/new.md`]: err(502, 'storage returned 500', { upstream: 500 }) } } }), { project: 'qa-alpha', name: 'new.md', text: 'QA', options: {} });
add('create-bad-name', [bare()], null, null, { project: 'qa-bare', name: 'bad.exe', text: 'QA', options: {} });
add('create-empty-name', [bare()], null, null, { project: 'qa-bare', name: '', text: 'QA', options: {} });
add('create-surrogates', [bare()], null, null, { project: 'qa-bare', name: 'emoji.md', text: 'QA \u{1f600} lone \ud800 end', options: {} });
add('create-missing-project', [bare()], null, null, { project: 'qa-none', name: 'new.md', text: 'QA', options: {} });
// In-place edits of a stored file.
add('edit-stored', [alpha()], DAV, stored(), editNotes('QA note: edited.'));
add('edit-stored-content-changed', [alpha()], DAV, stored(), editNotes('QA x', { expectContent: 'QA something else' }));
add('edit-stored-attachment-changed', [alpha()], DAV, stored(), editNotes('QA x', { expectAttachment: '0'.repeat(64) }));
add('edit-stored-attachment-undefined', [alpha()], DAV, stored(), (() => { const c = editNotes('QA x'); delete c.options.expectAttachment; return c; })());
add('edit-stored-gone-remote', [alpha()], DAV, stored({ files: {} }), editNotes('QA x'));
add('edit-stored-bytes-differ', [alpha()], DAV, stored({ files: { [`${F}/Text/notes.md`]: { text: 'QA changed there', etag: '"qa-etag-a"' } } }), editNotes('QA x'));
add('edit-stored-no-etag', [alpha()], DAV, stored({ files: { [`${F}/Text/notes.md`]: { text: NOTE, etag: '' } } }), editNotes('QA x'));
add('edit-stored-version-401', [alpha()], DAV, stored({ fail: { version: { [`${F}/Text/notes.md`]: err(502, 'storage returned 401', { upstream: 401 }) } } }), editNotes('QA x'));
add('edit-stored-version-folder', [alpha()], DAV, stored({ fail: { version: { [`${F}/Text/notes.md`]: err(409, 'is a folder', { code: 'folder' }) } } }), editNotes('QA x'));
add('edit-stored-version-500', [alpha()], DAV, stored({ fail: { version: { [`${F}/Text/notes.md`]: err(502, 'storage returned 500', { upstream: 500 }) } } }), editNotes('QA x'));
add('edit-stored-read-404', [alpha()], DAV, stored({ fail: { read: { [`${F}/Text/notes.md`]: err(404, 'not found', { upstream: 404 }) } } }), editNotes('QA x'));
add('edit-stored-write-412', [alpha()], DAV, stored({ fail: { write: { [`${F}/Text/notes.md`]: err(412, 'changed', { code: 'changed', upstream: 412 }) } } }), editNotes('QA x'));
add('edit-stored-write-unknown', [alpha()], DAV, stored({ fail: { write: { [`${F}/Text/notes.md`]: err(502, 'connection lost', { code: 'unknown' }) } } }), editNotes('QA x'));
add('edit-stored-disconnected', [alpha()], null, null, editNotes('QA x'));
add('edit-partial', [alpha()], DAV, stored(), { project: 'qa-alpha', name: 'partial.md', text: 'QA x', options: { expectName: `${F}/Text/partial.md`, expectContent: 'QA partial', expectAttachment: sha(Buffer.from('QA partial')) } });
add('edit-not-listed', [alpha()], DAV, stored(), { project: 'qa-alpha', name: 'gone.md', text: 'QA x', options: { expectName: `${F}/Text/gone.md`, expectContent: '' } });
add('edit-expect-empty', [alpha()], DAV, stored(), { project: 'qa-alpha', name: 'x.md', text: 'QA', options: { expectName: '' } });
add('edit-expect-number', [alpha()], DAV, stored(), { project: 'qa-alpha', name: 'x.md', text: 'QA', options: { expectName: 5 } });
// Local (storage disconnected) edits of a plain file.
add('edit-local-plain', [bare()], null, null, { project: 'qa-bare', name: 'plain.txt', text: 'QA edited', options: { expectName: 'plain.txt', expectContent: PLAIN, expectAttachment: null } });
add('edit-local-plain-content-differs', [bare()], null, null, { project: 'qa-bare', name: 'plain.txt', text: 'QA edited', options: { expectName: 'plain.txt', expectContent: 'QA other', expectAttachment: null } });
add('edit-plain-while-connected', [bare()], DAV, { files: {}, dirs: [], fail: {} }, { project: 'qa-bare', name: 'plain.txt', text: 'QA edited', options: { expectName: 'plain.txt', expectContent: PLAIN, expectAttachment: null } });
// Adopt (#687).
add('adopt-into-existing-folder', [alpha()], DAV, stored(), adoptPlain('qa-alpha'));
add('adopt-makes-folder', [bare()], DAV, { files: {}, dirs: [], fail: {} }, adoptPlain('qa-bare'));
add('adopt-other-account', [alpha()], OTHER_DAV, stored(), adoptPlain('qa-alpha'));
add('adopt-disconnected', [alpha()], null, null, adoptPlain('qa-alpha'));
add('adopt-target-exists', [alpha()], DAV, stored({ files: { [`${F}/Text/notes.md`]: { text: NOTE, etag: '"qa-etag-a"' }, [`${F}/Text/plain.txt`]: { text: 'QA there', etag: '"qa-etag-p"' } } }), adoptPlain('qa-alpha'));
add('adopt-wrong-target', [alpha()], DAV, stored(), adoptPlain('qa-alpha', { adoptTo: `${F}/Text/elsewhere.txt` }));
add('adopt-name-differs', [alpha()], DAV, stored(), { ...adoptPlain('qa-alpha'), name: 'other.txt' });
add('adopt-folder-fails', [bare()], DAV, { files: {}, dirs: [], fail: { mkdir: { [ROOT]: err(502, 'storage returned 503', { upstream: 503 }) } } }, adoptPlain('qa-bare'));
add('adopt-write-412', [alpha()], DAV, stored({ fail: { write: { [`${F}/Text/plain.txt`]: err(412, 'changed', { code: 'changed', upstream: 412 }) } } }), adoptPlain('qa-alpha'));

add('create-replaces-local', [bare()], null, null, { project: 'qa-bare', name: 'plain.txt', text: 'QA replaced', options: {} });
add('create-image', [bare()], null, null, { project: 'qa-bare', name: 'pic.png', text: 'QA not really an image', options: {} });
add('create-html', [bare()], null, null, { project: 'qa-bare', name: 'page.html', text: '<p>QA</p>', options: {} });
add('create-falsy-entries', [bare((p) => { p.files.unshift(null, 0, ''); p.sourceFolders = [`${ROOT}/x`, `${ROOT}/x`]; })], DAV, { files: {}, dirs: [ROOT, `${ROOT}/x`], fail: {} }, { project: 'qa-bare', name: 'new.md', text: 'QA', options: {} });
add('create-remote-dup-sources', [alpha((p) => { p.sourceFolders = [F, F, `${ROOT}/y`]; })], DAV, stored(), { project: 'qa-alpha', name: 'new.md', text: 'QA', options: {} });
add('adopt-null-target', [alpha()], DAV, stored(), adoptPlain('qa-alpha', { adoptTo: null }));
add('adopt-empty-account', [alpha()], DAV, stored(), adoptPlain('qa-alpha', { adoptAccount: '' }));
add('adopt-write-unknown', [alpha()], DAV, stored({ fail: { write: { [`${F}/Text/plain.txt`]: err(502, 'connection lost', { code: 'unknown' }) } } }), adoptPlain('qa-alpha'));
add('adopt-version-fails', [alpha()], DAV, stored({ fail: { version: { [`${F}/Text/plain.txt`]: err(502, 'storage returned 503', { upstream: 503 }) } } }), adoptPlain('qa-alpha'));
add('edit-stored-unsupported', [alpha()], DAV, stored({ fail: { version: { [`${F}/Text/notes.md`]: err(400, 'this storage cannot report a file version', { code: 'unsupported' }) } } }), editNotes('QA x'));
add('edit-stored-read-403', [alpha()], DAV, stored({ fail: { read: { [`${F}/Text/notes.md`]: err(502, 'storage returned 403', { upstream: 403 }) } } }), editNotes('QA x'));

// ── The scripted storage ─────────────────────────────────────────────────────
function scripted(conn, spec, calls) {
  const files = new Map(Object.entries((spec && spec.files) || {}));
  const dirs = new Set((spec && spec.dirs) || []);
  const fail = (spec && spec.fail) || {};
  let n = 0;
  const boom = (e) => Object.assign(new Error(e.message), { status: e.status, ...(e.code ? { code: e.code } : {}), ...(e.upstream ? { upstream: e.upstream } : {}) });
  const impl = {
    isBrowsable: (c) => !!c && ['webdav', 'nextcloud', 's3'].includes(c.kind),
    async fileVersion(_c, p) {
      calls.push(['version', p]);
      if (fail.version && fail.version[p]) throw boom(fail.version[p]);
      const f = files.get(p);
      return f ? { exists: true, etag: f.etag } : { exists: false, etag: '' };
    },
    async readBinaryFile(_c, p) {
      calls.push(['read', p]);
      if (fail.read && fail.read[p]) throw boom(fail.read[p]);
      const f = files.get(p);
      if (!f) throw boom({ status: 404, message: 'not found', upstream: 404 });
      return Buffer.from(f.text, 'utf8');
    },
    async createFolder(_c, p) {
      calls.push(['mkdir', p]);
      if (fail.mkdir && fail.mkdir[p]) throw boom(fail.mkdir[p]);
      const existed = dirs.has(p);
      dirs.add(p);
      return { existed };
    },
    async writeFile(_c, p, bytes, { ifMatch, ifNoneMatch } = {}) {
      calls.push(['write', p, ifMatch !== undefined ? `if-match ${ifMatch}` : ifNoneMatch !== undefined ? 'if-none-match' : '']);
      if (fail.write && fail.write[p]) throw boom(fail.write[p]);
      const f = files.get(p);
      if ((ifMatch !== undefined && (!f || f.etag !== ifMatch)) || (ifNoneMatch !== undefined && f)) throw boom({ status: 412, message: 'changed', code: 'changed', upstream: 412 });
      n += 1;
      const etag = `"qa-etag-${n}"`;
      files.set(p, { text: Buffer.from(bytes).toString('utf8'), etag });
      return { etag, created: !f };
    },
    async discardWritten(_c, p, etag) {
      calls.push(['discard', p]);
      const f = files.get(p);
      if (f && f.etag === etag) { files.delete(p); return { deleted: true, reason: 'deleted' }; }
      return { deleted: false, reason: 'changed' };
    },
    storageFailure: () => null,
    refusedLogin: () => false,
  };
  return impl;
}

function stubStore(root, writes) {
  const file = (user, name) => path.join(root, 'users', user, name);
  return {
    async read(user, name) { try { return { version: 1, text: fs.readFileSync(file(user, name), 'utf8') }; } catch { return { version: 1, text: null }; } },
    async write(user, name, _version, text) { writes.push({ name, text }); fs.writeFileSync(file(user, name), text); return { ok: true }; },
    grantContext() { throw Error('not used'); }, chatMove() { throw Error('not used'); },
  };
}

async function run(c) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'qa-text-write-'));
  const dir = path.join(root, 'users', USER);
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, 'projects.json'), c.projects);
  for (const [id, texts] of Object.entries(TEXTS)) {
    fs.mkdirSync(path.join(dir, 'project-sources', id), { recursive: true });
    for (const t of texts) fs.writeFileSync(path.join(dir, 'project-sources', id, `${ref(t)}.txt`), t);
  }
  const calls = [], writes = [], index = [];
  const storageClient = scripted(c.connection, c.storage, calls);
  const workspace = createWorkspaceStore(root, { id: 'local', label: 'QA local', baseUrl: '', apiKey: '' }, null, { projectStore: stubStore(root, writes) }).get(USER);
  const currentWorkspace = () => workspace;
  const proxy = (field) => new Proxy([], {
    get(_t, prop) { const v = workspace[field][prop]; return typeof v === 'function' ? v.bind(workspace[field]) : v; },
    set(_t, prop, v) { workspace[field][prop] = v; return true; },
    ownKeys() { return Reflect.ownKeys(workspace[field]); },
    getOwnPropertyDescriptor(_t, prop) { return Reflect.getOwnPropertyDescriptor(workspace[field], prop); },
  });
  const rag = {
    indexProjectFile: async (_p, name) => { index.push(['index', name]); return { ok: true, stored: 1, embedded: 1 }; },
    deleteProjectFile: (_p, name) => { index.push(['unindex', name]); },
  };
  const authService = { getStorage: () => (c.connection ? { ...c.connection, secret: 'qa-synthetic-secret' } : { kind: 'local', baseUrl: '', bucket: '', region: 'us-east-1', username: '' }) };
  const store = createProjectStore({ fs, path, reasoningEffort: {}, projectAppearance: (b) => b, rag, storageClient, documentSources: {}, authService, currentWorkspace,
    PROJECTS: proxy('projects'), FREE_CHATS: proxy('freeChats'), sanitizeToolboxes: (b) => b, defaultToolboxes: () => [], PROJECT_ROOT_FOLDER: ROOT, createProjectFolder, projectSweep: { afterDelete: async () => {} } });
  let answer;
  const writesBefore = writes.length;
  try {
    const project = store.getProject(c.call.project) || { id: c.call.project };
    const file = await store.writeProjectTextFile(project, c.call.name, c.call.text, c.call.options);
    // The record as the caller holds it: its text through the source accessor (core's save moved it
    // to its file), and the fields the write set.
    const view = { name: file.name, source: file.source, attachment: file.attachment, document: file.document, content: String(file.content || '') };
    answer = { file: JSON.stringify(view) };
  } catch (e) {
    answer = { error: String(e && e.message ? e.message : e) };
  }
  for (let i = 0; i < 10; i += 1) await new Promise((r) => setTimeout(r, 0));
  while (workspace.pendingStoreWrites()) await workspace.pendingStoreWrites().catch(() => {});
  const last = writes.slice(writesBefore).filter((w) => w.name === 'projects.json').pop();
  const saved = last ? (JSON.parse(last.text).projects || []).find((p) => p && p.id === c.call.project) : null;
  fs.rmSync(root, { recursive: true, force: true });
  return { ...answer, calls, project: saved ? JSON.stringify(saved) : null, index };
}

function sources() {
  const all = Object.keys(require.cache);
  if (all.some((k) => k.includes(`${path.sep}node_modules${path.sep}`))) throw Error('a module from node_modules was loaded');
  return Object.fromEntries(all.filter((k) => k.startsWith(core + path.sep)).map((k) => path.relative(core, k).split(path.sep).join('/')).sort()
    .map((f) => [f, sha(fs.readFileSync(path.join(core, f)))]));
}

(async () => {
  const recorded = [];
  for (const c of cases) recorded.push({ ...c, accounts: { [JSON.stringify(c.connection)]: c.connection ? account(c.connection) : null }, node: await run(c) });
  const doc = { version: 1, generator: 'tools/gen-text-write-fixtures.cjs', now: NOW, user: USER, root: ROOT, texts: TEXTS, wasmSha256: sha(fs.readFileSync(process.env.DAV_PARSE_WASM)), sourcesRead: sources(), cases: recorded };
  const text = `${pretty(doc)}\n`;
  if (checkAt) {
    if (fs.readFileSync(checkAt, 'utf8') !== text) { process.stderr.write(`${checkAt} differs from a fresh recording\n`); process.exit(1); }
    process.stderr.write(`${checkAt}: identical\n`);
  } else fs.writeFileSync(out, text);
})().catch((e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
