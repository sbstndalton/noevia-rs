#!/usr/bin/env node
// Test-oracle glue only: runs noevia-core's own in-app MCP server (mcp-internal.cjs createHandler, a
// token minted with its mintToken) over its own project write tools (mcp-internal-tools.cjs
// project_create_file, project_append_file, project_replace_text, over project-edit-target.cjs and
// the dav-parse.wasm core pins) on synthetic QA projects, and records per `tools/call` the result the
// server answers and the write the handler asked for (`ports.writeTextFile(project, name, text,
// options)`, answered by the script: done, or a thrown message). Track R14 step 6:
// crates/edit-target-lookup/tests/node_inapp.rs replays every case against `inapp::prepare`/`finish`.
//
//   DAV_PARSE_WASM=<dav-parse.wasm whose sha256 is core's server/dav-parse.lock pin> \
//     node tools/gen-inapp-writes-fixtures.cjs <noevia-core checkout> <output.json>
//   ... node tools/gen-inapp-writes-fixtures.cjs <noevia-core checkout> --check <fixture.json>
//
// A source's text is inline (`content`). Each core file loaded is listed with its sha256.
'use strict';
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

const core = path.resolve(process.argv[2] || '');
const checkAt = process.argv[3] === '--check' ? process.argv[4] : null;
const out = checkAt ? null : process.argv[3];
if (!process.argv[2] || (!out && !checkAt)) throw Error('usage: gen-inapp-writes-fixtures.cjs <core> <output.json> | --check <fixture.json>');
if (!process.env.DAV_PARSE_WASM) throw Error('DAV_PARSE_WASM must name the pinned dav-parse.wasm');
for (const k of ['log', 'info', 'warn', 'error', 'debug']) console[k] = () => {};
const req = (m) => require(path.join(core, 'server', m));
const { mintToken, createHandler } = req('mcp-internal.cjs');
const { createInternalTools } = req('mcp-internal-tools.cjs');
const edits = req('project-edit-target.cjs');
const pretty = (v) => JSON.stringify(v, null, 2);
const sha = (b) => crypto.createHash('sha256').update(b).digest('hex');

const KEY = Buffer.alloc(32, 9);
const USER = 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee';
const F = 'noevia projects/QA Alpha';
const ACCOUNT = 'qa-account-digest-1';
const NOTE = 'QA line one\nQA line two\nQA two again two';
const alpha = {
  id: 'qa-alpha', projectFolder: F,
  files: [
    { name: `${F}/Text/notes.md`, source: F, content: NOTE, attachment: { id: sha(Buffer.from(NOTE)), bytes: NOTE.length, group: 'Text', state: 'ready' } },
    { name: 'plain.txt', content: 'QA plain' },
    { name: 'empty.txt', content: '' },
    { name: 'nl.txt', content: 'QA ends\n' },
    { name: 'astral.txt', content: 'QA \u{1f600} x \u{1f600}' },
    { name: 'orig.pdf', attachment: { state: 'stored', group: 'Documents' } },
    { name: 'x/dup.md', content: 'a' }, { name: 'y/dup.md', content: 'b' },
  ],
};
const bare = { id: 'qa-bare', files: [{ name: 'plain.txt', content: 'QA plain bare' }] };
const withNull = { id: 'qa-null', files: [null, { name: 'a.txt', content: 'x' }] };
const filesString = { id: 'qa-str', files: 'nope' };
const folderNumber = { id: 'qa-num', projectFolder: 7, files: [] };
const PROJECTS = { 'qa-alpha': alpha, 'qa-bare': bare, 'qa-null': withNull, 'qa-str': filesString, 'qa-num': folderNumber };

const cases = [];
const add = (name, tool, pid, args, { pin = 'auto', account = null, writeFails = null } = {}) => cases.push({ name, tool, pid, args, pin, account, writeFails });
// project_create_file
add('create', 'project_create_file', 'qa-bare', { name: 'new.md', text: 'QA new' });
add('create-path-name', 'project_create_file', 'qa-bare', { name: 'dir/sub\\deep.md ', text: 'QA' });
add('create-dot', 'project_create_file', 'qa-bare', { name: '..', text: 'QA' });
add('create-empty', 'project_create_file', 'qa-bare', { name: '', text: 'QA' });
add('create-no-name', 'project_create_file', 'qa-bare', { text: 'QA' });
add('create-number-name', 'project_create_file', 'qa-bare', { name: 42, text: 7 });
add('create-null-text', 'project_create_file', 'qa-bare', { name: 'n.md', text: null });
add('create-object-text', 'project_create_file', 'qa-bare', { name: 'o.md', text: { a: 1 } });
add('create-tostring-name', 'project_create_file', 'qa-bare', { name: { toString: 1 }, text: 'x' });
add('create-long-name', 'project_create_file', 'qa-bare', { name: `${'n'.repeat(250)}.md`, text: 'x' });
add('create-exists', 'project_create_file', 'qa-bare', { name: 'plain.txt', text: 'x' });
add('create-exists-upload', 'project_create_file', 'qa-alpha', { name: 'notes.md', text: 'x' });
add('create-too-large', 'project_create_file', 'qa-bare', { name: 'big.md', text: 'é'.repeat(131073) });
add('create-just-fits', 'project_create_file', 'qa-bare', { name: 'fits.md', text: 'é'.repeat(131072) });
add('create-astral', 'project_create_file', 'qa-bare', { name: 'a.md', text: 'QA \u{1f600}\u{1f600}' });
add('create-null-entry', 'project_create_file', 'qa-null', { name: 'z.md', text: 'x' });
add('create-write-fails', 'project_create_file', 'qa-bare', { name: 'f.md', text: 'x' }, { writeFails: 'storage returned 500' });
add('create-write-fails-long', 'project_create_file', 'qa-bare', { name: 'f.md', text: 'x' }, { writeFails: `QA ${'long '.repeat(80)}\u{1f600}` });
add('create-no-project', 'project_create_file', 'qa-none', { name: 'x.md', text: 'x' });
add('create-no-pid', 'project_create_file', null, { name: 'x.md', text: 'x' });
add('create-args-list', 'project_create_file', 'qa-bare', ['x']);
// project_append_file
add('append', 'project_append_file', 'qa-alpha', { name: 'plain.txt', text: 'QA more' });
add('append-after-newline', 'project_append_file', 'qa-alpha', { name: 'nl.txt', text: 'QA more' });
add('append-to-empty', 'project_append_file', 'qa-alpha', { name: 'empty.txt', text: 'QA first' });
add('append-stored', 'project_append_file', 'qa-alpha', { name: 'notes.md', text: 'QA more' }, { account: ACCOUNT });
add('append-adopt', 'project_append_file', 'qa-alpha', { name: 'plain.txt', text: 'QA more' }, { account: ACCOUNT });
add('append-nothing', 'project_append_file', 'qa-alpha', { name: 'plain.txt', text: '' });
add('append-null-text', 'project_append_file', 'qa-alpha', { name: 'plain.txt' });
add('append-no-pin', 'project_append_file', 'qa-alpha', { name: 'plain.txt', text: 'x' }, { pin: 'none' });
add('append-wrong-pin', 'project_append_file', 'qa-alpha', { name: 'plain.txt', text: 'x' }, { pin: 'wrong' });
add('append-missing', 'project_append_file', 'qa-alpha', { name: 'gone.txt', text: 'x' });
add('append-ambiguous', 'project_append_file', 'qa-alpha', { name: 'dup.md', text: 'x' }, { pin: 'wrong' });
add('append-original', 'project_append_file', 'qa-alpha', { name: 'orig.pdf', text: 'x' }, { pin: 'wrong' });
add('append-too-large', 'project_append_file', 'qa-alpha', { name: 'plain.txt', text: 'x'.repeat(262140) });
add('append-write-fails', 'project_append_file', 'qa-alpha', { name: 'plain.txt', text: 'x' }, { writeFails: '"plain.txt" changed while this edit was being made; nothing was saved. Read it again and retry.' });
add('append-no-project', 'project_append_file', 'qa-none', { name: 'plain.txt', text: 'x' }, { pin: 'wrong' });
add('append-files-string', 'project_append_file', 'qa-str', { name: 'a.txt', text: 'x' }, { pin: 'wrong' });
// project_replace_text
add('replace', 'project_replace_text', 'qa-alpha', { name: 'plain.txt', find: 'plain', replace: 'PLAIN' });
add('replace-two', 'project_replace_text', 'qa-alpha', { name: 'notes.md', find: 'two', replace: '2', expectedCount: 3 }, { account: ACCOUNT });
add('replace-count-wrong', 'project_replace_text', 'qa-alpha', { name: 'notes.md', find: 'two', replace: '2' }, { account: ACCOUNT });
add('replace-count-string', 'project_replace_text', 'qa-alpha', { name: 'notes.md', find: 'two', replace: '2', expectedCount: ' 3 ' }, { account: ACCOUNT });
add('replace-count-two-wrong', 'project_replace_text', 'qa-alpha', { name: 'notes.md', find: 'line', replace: 'L', expectedCount: 3 }, { account: ACCOUNT });
add('replace-not-found', 'project_replace_text', 'qa-alpha', { name: 'plain.txt', find: 'absent', replace: 'x' });
add('replace-empty-find', 'project_replace_text', 'qa-alpha', { name: 'plain.txt', find: '', replace: 'x' });
add('replace-zero', 'project_replace_text', 'qa-alpha', { name: 'plain.txt', find: 'plain', replace: 'x', expectedCount: 0 });
add('replace-fraction', 'project_replace_text', 'qa-alpha', { name: 'plain.txt', find: 'plain', replace: 'x', expectedCount: 1.5 });
add('replace-bool', 'project_replace_text', 'qa-alpha', { name: 'plain.txt', find: 'plain', replace: 'x', expectedCount: true });
add('replace-null-count', 'project_replace_text', 'qa-alpha', { name: 'plain.txt', find: 'plain', replace: 'x', expectedCount: null });
add('replace-null-replace', 'project_replace_text', 'qa-alpha', { name: 'plain.txt', find: 'plain', replace: null });
add('replace-astral', 'project_replace_text', 'qa-alpha', { name: 'astral.txt', find: '\u{1f600}', replace: '!', expectedCount: 2 });
add('replace-overlap', 'project_replace_text', 'qa-alpha', { name: 'notes.md', find: 'o', replace: 'oo', expectedCount: 4 }, { account: ACCOUNT });
add('replace-no-pin', 'project_replace_text', 'qa-alpha', { name: 'plain.txt', find: 'plain', replace: 'x' }, { pin: 'none' });
add('replace-adopt', 'project_replace_text', 'qa-alpha', { name: 'plain.txt', find: 'plain', replace: 'x' }, { account: ACCOUNT });
add('replace-folder-number', 'project_replace_text', 'qa-num', { name: 'a.txt', find: 'x', replace: 'y' }, { pin: 'wrong' });

async function run(c) {
  const writes = [];
  const ports = {
    cap: 8000,
    getProject: (id) => (id && Object.hasOwn(PROJECTS, id) ? JSON.parse(JSON.stringify(PROJECTS[id])) : null),
    storageAccount: () => c.account,
    writeTextFile: async (_project, name, text, options) => {
      writes.push({ name, text, options: options === undefined ? null : options });
      if (c.writeFails) throw new Error(c.writeFails);
      return { name };
    },
    diary: async () => ({}), readProjectFile: async () => '', ragAvailable: () => false, search: async () => [],
  };
  const handle = createHandler({ key: KEY, definitions: createInternalTools(ports) });
  let pin = null;
  if (c.pin === 'wrong') pin = '0'.repeat(64);
  else if (c.pin === 'auto') {
    const project = ports.getProject(c.pid);
    try { const plan = edits.planEdit(project, c.args.name, { storageAccount: c.account }); pin = edits.targetDigest(plan.target, plan.account); } catch { pin = null; }
  }
  const token = mintToken(KEY, { uid: USER, pid: c.pid, w: 1, t: pin });
  const res = await handle({ jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name: c.tool, arguments: c.args } }, token);
  return { status: res.status, result: res.body.result ?? null, error: res.body.error ?? null, pin, writes };
}

function sources() {
  const all = Object.keys(require.cache);
  if (all.some((k) => k.includes(`${path.sep}node_modules${path.sep}`))) throw Error('a module from node_modules was loaded');
  return Object.fromEntries(all.filter((k) => k.startsWith(core + path.sep)).map((k) => path.relative(core, k).split(path.sep).join('/')).sort()
    .map((f) => [f, sha(fs.readFileSync(path.join(core, f)))]));
}

(async () => {
  const recorded = [];
  for (const c of cases) {
    const node = await run(c);
    recorded.push({ name: c.name, tool: c.tool, pid: c.pid, args: JSON.stringify(c.args), account: c.account, writeFails: c.writeFails, project: c.pid && Object.hasOwn(PROJECTS, c.pid) ? JSON.stringify(PROJECTS[c.pid]) : null, node: { ...node, result: node.result === null ? null : JSON.stringify(node.result), writes: node.writes.map((w) => JSON.stringify(w)) } });
  }
  const doc = { version: 1, generator: 'tools/gen-inapp-writes-fixtures.cjs', wasmSha256: sha(fs.readFileSync(process.env.DAV_PARSE_WASM)), sourcesRead: sources(), cases: recorded };
  const text = `${pretty(doc)}\n`;
  if (checkAt) {
    if (fs.readFileSync(checkAt, 'utf8') !== text) { process.stderr.write(`${checkAt} differs from a fresh recording\n`); process.exit(1); }
    process.stderr.write(`${checkAt}: identical\n`);
  } else fs.writeFileSync(out, text);
})().catch((e) => { process.stderr.write(`${e.stack}\n`); process.exit(1); });
