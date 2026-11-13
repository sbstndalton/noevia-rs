'use strict';
// Writes tests/fixtures/files-node.v1.json: what noevia-core's own code-harness.cjs `defaultFiles`
// (the Code agent's only way to read and write through noevia: pinnedParent, verifyOpened, the
// mkdir walk) answers for scripted reads and writes in a synthetic tree full of links, hard links
// and directories, and what the tree holds after each. tests/files.rs replays the script through
// the Rust port and compares.
//
// Usage: node crates/code-harness/tools/gen-files-fixtures.cjs <noevia-core>/server > crates/code-harness/tests/fixtures/files-node.v1.json
// Synthetic data only. Paths are relative to a fresh base directory (`<BASE>` in the fixture).

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');

const serverDir = path.resolve(process.argv[2] || '');
const { defaultFiles } = require(path.join(serverDir, 'code-harness.cjs'));

// Setup ops: { dir }, { file, text }, { link, to } (to is relative to the base unless absolute-ish
// with <BASE>), { hard, to }. Steps: { read, max } / { write, text } with `root` (default 'tree';
// null for none) and the target relative to the base.
const SETUP = [
  { dir: 'tree' }, { dir: 'tree/sub' }, { dir: 'outside' }, { dir: 'outside/deep' },
  { file: 'tree/a.txt', text: 'qa a\n' }, { file: 'tree/big.txt', text: 'x'.repeat(100) },
  { file: 'tree/utf8.txt', text: 'é😀\n' }, { file: 'outside/secret.txt', text: 'synthetic secret' },
  { file: 'tree/linked.txt', text: 'linked' }, { hard: 'outside/other-name.txt', to: 'tree/linked.txt' },
  { link: 'tree/to-outside-file', to: '<BASE>/outside/secret.txt' },
  { link: 'tree/dangling', to: '<BASE>/outside/authorized_keys' },
  { link: 'tree/to-inside-file', to: '<BASE>/tree/a.txt' },
  { link: 'tree/dirlink-out', to: '<BASE>/outside' },
  { link: 'tree/dirlink-in', to: '<BASE>/tree/sub' },
  { link: 'tree/dangling-dir', to: '<BASE>/nowhere/at/all' },
  { link: 'treelink', to: '<BASE>/tree' },
];

const STEPS = [
  { read: 'tree/a.txt', max: 1000 },
  { read: 'tree/big.txt', max: 99 },
  { read: 'tree/big.txt', max: 100 },
  { read: 'tree/utf8.txt', max: 100 },
  { read: 'tree/missing.txt', max: 100 },
  { read: 'tree/sub', max: 100 },
  { read: 'tree/linked.txt', max: 100 },
  { read: 'tree/to-outside-file', max: 100 },
  { read: 'tree/to-inside-file', max: 100 },
  { read: 'tree/dangling', max: 100 },
  { read: 'tree/dirlink-out/secret.txt', max: 100 },
  { read: 'tree/dirlink-in/x.txt', max: 100 },
  { read: 'outside/secret.txt', max: 100 },
  { read: 'tree/a.txt', max: 100, root: null },
  { read: 'tree/a.txt', max: 100, root: 'treelink' },
  { read: 'treelink/a.txt', max: 100 },
  { read: 'tree/../outside/secret.txt', max: 100 },
  { write: 'tree/new.txt', text: 'new file\n' },
  { write: 'tree/a.txt', text: 'shorter' },
  { write: 'tree/new/deep/ok.txt', text: 'deep' },
  { write: 'tree/sub/x/y.txt', text: 'under sub' },
  { write: 'tree/linked.txt', text: 'overwritten' },
  { write: 'tree/to-outside-file', text: 'x' },
  { write: 'tree/to-inside-file', text: 'x' },
  { write: 'tree/dangling', text: 'ssh-ed25519 AAAA' },
  { write: 'tree/dirlink-out/new.txt', text: 'x' },
  { write: 'tree/dirlink-out/a/b/file', text: 'x' },
  { write: 'tree/dirlink-in/inside.txt', text: 'via an inside link' },
  { write: 'tree/dirlink-in/made/here.txt', text: 'x' },
  { write: 'tree/dangling-dir/x.txt', text: 'x' },
  { write: 'tree/sub', text: 'x' },
  { write: 'outside/x.txt', text: 'x' },
  { write: 'tree/x.txt', text: 'x', root: null },
  { write: 'tree/x.txt', text: 'x', root: 'missing-root' },
  { write: 'tree/../outside/escape.txt', text: 'x' },
  { write: 'tree/a.txt/child.txt', text: 'x' },
  { write: 'tree/empty.txt', text: '' },
  { write: 'tree/lone.txt', text: 'lone \ud800 surrogate' },
];

function walk(base, rel = '') {
  const out = [];
  let entries = [];
  try { entries = fs.readdirSync(path.join(base, rel), { withFileTypes: true }); } catch { return out; }
  for (const e of entries.sort((a, b) => (Buffer.from(a.name) < Buffer.from(b.name) ? -1 : 1))) {
    const r = rel ? `${rel}/${e.name}` : e.name;
    const full = path.join(base, r);
    if (e.isSymbolicLink()) out.push(`${r} -> ${fs.readlinkSync(full).split(base).join('<BASE>')}`);
    else if (e.isDirectory()) { out.push(`${r}/`); out.push(...walk(base, r)); }
    else out.push(`${r} = ${JSON.stringify(fs.readFileSync(full, 'utf8'))} (${(fs.statSync(full).mode & 0o777).toString(8)})`);
  }
  return out;
}

const base = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'qa-harness-files-')));
const at = (p) => path.join(base, p);
const sub = (t) => String(t).split('<BASE>').join(base);
process.umask(0o022);
for (const s of SETUP) {
  if (s.dir) fs.mkdirSync(at(s.dir), { recursive: true });
  else if (s.file) fs.writeFileSync(at(s.file), s.text);
  else if (s.link) fs.symlinkSync(sub(s.to), at(s.link));
  else if (s.hard) fs.linkSync(at(s.to), at(s.hard));
}
const results = [];
for (const s of STEPS) {
  const root = s.root === undefined ? at('tree') : s.root === null ? undefined : at(s.root);
  let result;
  try {
    const value = s.read !== undefined ? defaultFiles.read(at(s.read), s.max, root) : (defaultFiles.write(at(s.write), s.text, root), null);
    result = { ok: value === undefined ? null : value };
  } catch (e) {
    result = { error: { code: e.code === -32602 ? -32602 : null, message: e.code === -32602 ? e.message : '<os error>' } };
  }
  results.push({ ...s, result, tree: walk(base) });
}
fs.rmSync(base, { recursive: true, force: true });
const sha = crypto.createHash('sha256').update(fs.readFileSync(path.join(serverDir, 'code-harness.cjs'))).digest('hex');
process.stdout.write(JSON.stringify({ generator: 'crates/code-harness/tools/gen-files-fixtures.cjs', sources: { 'code-harness.cjs': sha },
  umask: '022', setup: SETUP, steps: results }, null, 1) + '\n');
