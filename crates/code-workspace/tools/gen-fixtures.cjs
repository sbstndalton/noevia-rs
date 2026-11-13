'use strict';
// Writes tests/fixtures/node.v1.json: what noevia-core's own server/code-workspace.cjs answers, and
// what it leaves on disk, for scripted synthetic operations on throwaway git repositories. The Rust
// test (tests/replay.rs) runs the same script through the code-workspace crate in a fresh directory
// with the same git and compares every answer and every snapshot.
//
// Usage: node crates/code-workspace/tools/gen-fixtures.cjs <noevia-core>/server > crates/code-workspace/tests/fixtures/node.v1.json
// Synthetic data only: QA-named repositories, fixed ids, fixed commit dates, a clock the script sets that
// also moves on by one at every reading (so no two records share a time).
//
// Normalised in both: the scenario's base directory is `<BASE>`; uid/gid numbers are `<uid>`/`<gid>`;
// git's own words after `Command failed: <command>\n` are `<stderr>` (they differ between git versions).

const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');

const serverDir = path.resolve(process.argv[2] || '');
const { createCodeWorkspaces, gitEnv } = require(path.join(serverDir, 'code-workspace.cjs'));

// Commit dates fixed for every git call, so every commit id is the same on every run.
const DATES = { GIT_AUTHOR_DATE: '2026-01-01T00:00:00Z', GIT_COMMITTER_DATE: '2026-01-01T00:00:00Z' };
// code-workspace.cjs's own defaultRun, with the dates added to the environment it is given.
function run(args, cwd, env = {}, { maxBuffer, raw = false } = {}) {
  const out = execFileSync('git', args, { cwd, env: gitEnv({ ...env, ...DATES }), encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'], timeout: 120_000, ...(maxBuffer ? { maxBuffer } : {}) });
  return raw ? out : out.trim();
}
// The script's own git (setting up repositories, acting as the harness): hermetic, fixed identity.
const HERMETIC = { ...DATES, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null',
  GIT_AUTHOR_NAME: 'QA', GIT_AUTHOR_EMAIL: 'qa@example.invalid', GIT_COMMITTER_NAME: 'QA', GIT_COMMITTER_EMAIL: 'qa@example.invalid' };
const shell = (args, cwd) => execFileSync('git', args, { cwd, env: { PATH: process.env.PATH, HOME: '/nonexistent', ...HERMETIC }, stdio: ['ignore', 'pipe', 'pipe'], encoding: 'utf8' });

const id = (n) => `00000000-0000-4000-8000-${String(n).padStart(12, '0')}`;
const T1 = id(1), T2 = id(2), T3 = id(3), T4 = id(4), T5 = id(5), T6 = id(6);
const WT = (t) => `tenant/code-workspaces/${t}`; // a worktree-mode tree
const CL = (t) => `trees/${t}`; // a tree under the shared root

const SCENARIOS = {
  'worktree': [
    { op: 'repo', name: 'qa-repo', files: { 'a.txt': 'a\n', 'README.md': '# QA\n' } },
    { op: 'dir', path: 'not-a-repo' },
    { op: 'ws', name: 'w', epoch: 'qa-epoch-1' },
    { op: 'clock', t: 1000 },
    { op: 'claim', ws: 'w', taskId: T1, repo: 'repos/qa-repo', capabilities: ['read', 'edit'], domains: ['example.invalid'] },
    { op: 'claim', ws: 'w', taskId: T1, repo: 'repos/qa-repo' },
    { op: 'claim', ws: 'w', taskId: 'not-an-id', repo: 'repos/qa-repo' },
    { op: 'claim', ws: 'w', taskId: T2, repo: 'repos/missing' },
    { op: 'claim', ws: 'w', taskId: T2, repo: 'not-a-repo' },
    { op: 'claim', ws: 'w', taskId: T2, repo: 'repos/qa-repo', branch: '-x' },
    { op: 'claim', ws: 'w', taskId: T2, repo: 'repos/qa-repo', branch: 'a..b' },
    { op: 'claim', ws: 'w', taskId: T2, repo: 'repos/qa-repo', branch: 'feature/x' },
    { op: 'claim', ws: 'w', taskId: T3, repo: 'repos/qa-repo', branch: 'feature/x' },
    { op: 'headSha', ws: 'w', taskId: T1 },
    { op: 'write', path: `${WT(T1)}/b.txt`, text: 'b\n' },
    { op: 'git', cwd: WT(T1), args: ['add', 'b.txt'] },
    { op: 'git', cwd: WT(T1), args: ['commit', '-qm', 'QA change'] },
    { op: 'headSha', ws: 'w', taskId: T1 },
    { op: 'headSha', ws: 'w', taskId: T6 },
    { op: 'dir', path: 'outside' },
    { op: 'write', path: 'outside/secret.txt', text: 'x' },
    { op: 'symlink', target: '<BASE>/outside', path: `${WT(T1)}/link` },
    { op: 'symlink', target: '<BASE>/nowhere', path: `${WT(T1)}/dangling` },
    { op: 'symlink', target: 'a.txt', path: `${WT(T1)}/inner-link` },
    ...[`<BASE>/${WT(T1)}/a.txt`, `<BASE>/${WT(T1)}/new/deep/file.txt`, '<BASE>/outside/secret.txt',
      `<BASE>/${WT(T1)}/../escape.txt`, '/etc/passwd', `<BASE>/${WT(T1)}/link/secret.txt`, `<BASE>/${WT(T1)}/link/brand-new.txt`,
      `<BASE>/${WT(T1)}/dangling`, `<BASE>/${WT(T1)}/inner-link`, 'a.txt', 'new/x.txt', '../x', '', `<BASE>/${WT(T1)}`,
      `<BASE>/${WT(T1)}/./a.txt`, `<BASE>/${WT(T1)}/new/../a.txt`, `<BASE>/${WT(T1)}x/a.txt`]
      .map((candidate) => ({ op: 'contains', ws: 'w', taskId: T1, candidate })),
    { op: 'contains', ws: 'w', taskId: T6, candidate: 'a.txt' },
    { op: 'clock', t: 2000 },
    { op: 'release', ws: 'w', taskId: T1 },
    { op: 'release', ws: 'w', taskId: T1 },
    { op: 'contains', ws: 'w', taskId: T1, candidate: 'a.txt' },
    { op: 'headSha', ws: 'w', taskId: T1 },
    { op: 'release', ws: 'w', taskId: T2, removeBranch: true },
    { op: 'claim', ws: 'w', taskId: T3, repo: 'repos/qa-repo', branch: 'feature/x' },
    { op: 'release', ws: 'w', taskId: T6 },
    // An empty branch is no branch: the task's own.
    { op: 'claim', ws: 'w', taskId: T4, repo: 'repos/qa-repo', branch: '' },
    { op: 'release', ws: 'w', taskId: T4 },
    { op: 'get', ws: 'w', taskId: T3 },
    { op: 'get', ws: 'w', taskId: T6 },
    { op: 'get', ws: 'w', taskId: 'bad' },
    { op: 'list', ws: 'w' },
  ],
  'empty-repo': [
    { op: 'repo', name: 'qa-empty', empty: true },
    { op: 'ws', name: 'w', epoch: 'qa-epoch-1' },
    { op: 'clock', t: 1000 },
    { op: 'claim', ws: 'w', taskId: T1, repo: 'repos/qa-empty' },
    { op: 'headSha', ws: 'w', taskId: T1 },
    { op: 'release', ws: 'w', taskId: T1 },
  ],
  'pins': [
    { op: 'repo', name: 'qa-repo', files: { 'a.txt': 'a\n' } },
    { op: 'repo', name: 'qa-tracks-pin', files: { 'a.txt': 'a\n', 'opencode.json': '{}\n' } },
    { op: 'ws', name: 'w', epoch: 'qa-epoch-1' },
    { op: 'clock', t: 1000 },
    { op: 'claim', ws: 'w', taskId: T1, repo: 'repos/qa-tracks-pin', pinned: ['opencode.json'] },
    { op: 'claim', ws: 'w', taskId: T1, repo: 'repos/qa-repo', pinned: ['opencode.json', '.claude/settings.local.json', '../bad', '/abs', 'a b', 7, '', 'x/../y'] },
    { op: 'write', path: `${WT(T1)}/opencode.json`, text: '{"key":"qa"}\n' },
    { op: 'dir', path: `${WT(T1)}/.claude` },
    { op: 'write', path: `${WT(T1)}/.claude/settings.local.json`, text: '{}\n' },
    { op: 'release', ws: 'w', taskId: T1 },
    { op: 'claim', ws: 'w', taskId: T2, repo: 'repos/qa-repo', pinned: ['opencode.json', '.claude/settings.local.json'] },
    { op: 'dir', path: 'outside' },
    { op: 'write', path: 'outside/settings.local.json', text: 'keep me\n' },
    { op: 'symlink', target: '<BASE>/outside', path: `${WT(T2)}/.claude` },
    { op: 'release', ws: 'w', taskId: T2 },
    { op: 'claim', ws: 'w', taskId: T3, repo: 'repos/qa-repo', pinned: ['opencode.json'] },
    { op: 'dir', path: `${WT(T3)}/opencode.json` },
    { op: 'release', ws: 'w', taskId: T3 },
  ],
  'recover': [
    { op: 'repo', name: 'qa-repo', files: { 'a.txt': 'a\n' } },
    { op: 'ws', name: 'old', epoch: 'qa-epoch-1' },
    { op: 'clock', t: 1000 },
    { op: 'claim', ws: 'old', taskId: T1, repo: 'repos/qa-repo', pinned: ['opencode.json'] },
    { op: 'write', path: `${WT(T1)}/opencode.json`, text: '{"key":"qa"}\n' },
    { op: 'claim', ws: 'old', taskId: T2, repo: 'repos/qa-repo', pinned: ['.claude/settings.local.json'] },
    { op: 'dir', path: 'outside' },
    { op: 'write', path: 'outside/settings.local.json', text: 'keep me\n' },
    { op: 'symlink', target: '<BASE>/outside', path: `${WT(T2)}/.claude` },
    { op: 'claim', ws: 'old', taskId: T3, repo: 'repos/qa-repo' },
    { op: 'release', ws: 'old', taskId: T3 },
    { op: 'ws', name: 'new', epoch: 'qa-epoch-2' },
    { op: 'clock', t: 5000 },
    { op: 'recover', ws: 'new' },
    { op: 'recover', ws: 'new' },
    { op: 'recover', ws: 'old' },
    { op: 'claim', ws: 'new', taskId: T4, repo: 'repos/qa-repo', branch: `noevia/task-${T1}` },
    { op: 'list', ws: 'new' },
  ],
  'clone': [
    { op: 'repo', name: 'qa-repo', files: { 'a.txt': 'a\n', 'README.md': '# QA\n' } },
    { op: 'ws', name: 'c', epoch: 'qa-epoch-1', treeRoot: 'trees', owner: 'self' },
    { op: 'clock', t: 1000 },
    { op: 'claim', ws: 'c', taskId: T1, repo: 'repos/qa-repo', pinned: ['opencode.json'] },
    { op: 'headSha', ws: 'c', taskId: T1 },
    { op: 'contains', ws: 'c', taskId: T1, candidate: `<BASE>/${CL(T1)}/a.txt` },
    { op: 'write', path: `${CL(T1)}/a.txt`, text: 'edited by the harness\n' },
    { op: 'write', path: `${CL(T1)}/opencode.json`, text: '{"key":"qa"}\n' },
    { op: 'git', cwd: CL(T1), args: ['add', '-f', 'opencode.json'] },
    { op: 'dir', path: `${CL(T1)}/.cache` },
    { op: 'write', path: `${CL(T1)}/.cache/state.db`, text: 'harness state\n' },
    { op: 'clock', t: 2000 },
    { op: 'release', ws: 'c', taskId: T1 },
    { op: 'release', ws: 'c', taskId: T1 },
    { op: 'claim', ws: 'c', taskId: T2, repo: 'repos/qa-repo' },
    { op: 'write', path: `${CL(T2)}/b.txt`, text: 'b\n' },
    { op: 'git', cwd: CL(T2), args: ['add', 'b.txt'] },
    { op: 'git', cwd: CL(T2), args: ['commit', '-qm', 'QA harness commit'] },
    { op: 'release', ws: 'c', taskId: T2, removeBranch: true },
    { op: 'claim', ws: 'c', taskId: T3, repo: 'repos/qa-repo' },
    { op: 'release', ws: 'c', taskId: T3 },
  ],
  'hostile-clone': [
    { op: 'repo', name: 'qa-repo', files: { 'a.txt': 'a\n' } },
    { op: 'ws', name: 'c', epoch: 'qa-epoch-1', treeRoot: 'trees', owner: 'self' },
    { op: 'clock', t: 1000 },
    ...[[T1, ['config', 'core.fsmonitor', 'touch <BASE>/pwned']], [T2, ['config', 'filter.x.clean', 'sh -c true']],
      [T3, ['config', 'alias.st', '!sh']], [T4, ['config', 'credential.helper', '!sh']]].flatMap(([t, args]) => [
      { op: 'claim', ws: 'c', taskId: t, repo: 'repos/qa-repo', branch: `qa/${t}` },
      { op: 'git', cwd: CL(t), args },
      { op: 'write', path: `${CL(t)}/a.txt`, text: 'changed\n' },
      { op: 'release', ws: 'c', taskId: t },
    ]),
    { op: 'claim', ws: 'c', taskId: T5, repo: 'repos/qa-repo', branch: 'qa/hook' },
    { op: 'write', path: `${CL(T5)}/.git/hooks/post-commit`, text: '#!/bin/sh\ntouch <BASE>/pwned\n', mode: 0o755 },
    { op: 'release', ws: 'c', taskId: T5 },
    { op: 'claim', ws: 'c', taskId: T6, repo: 'repos/qa-repo', branch: 'qa/attr' },
    { op: 'dir', path: `${CL(T6)}/deep` },
    { op: 'write', path: `${CL(T6)}/deep/.gitattributes`, text: '*.txt filter=x\n' },
    { op: 'release', ws: 'c', taskId: T6 },
    { op: 'release', ws: 'c', taskId: T6 },
    { op: 'list', ws: 'c' },
  ],
  'pipeline-worktree': [
    { op: 'repo', name: 'qa-repo', files: { 'a.txt': 'a\n', 'README.md': '# QA readme\n' + 'é'.repeat(10) + '\n', 'z.txt': 'z\n' } },
    { op: 'ws', name: 'w', epoch: 'qa-epoch-1' },
    { op: 'ws', name: 'tiny', epoch: 'qa-epoch-1', changeMaxBuffer: 100 },
    { op: 'clock', t: 1000 },
    { op: 'claim', ws: 'w', taskId: T1, repo: 'repos/qa-repo', pinned: ['opencode.json'] },
    { op: 'projectSnapshot', ws: 'w', taskId: T1 },
    { op: 'projectSnapshot', ws: 'w', taskId: T1, maxFiles: 1, maxReadmeBytes: 13 },
    { op: 'projectSnapshot', ws: 'w', taskId: T6 },
    { op: 'write', path: `${WT(T1)}/b.txt`, text: 'b\n' },
    { op: 'write', path: `${WT(T1)}/a.txt`, text: 'a\nmore\r\ndiff --git a/fake b/fake\n' },
    { op: 'write', path: `${WT(T1)}/new\nline.txt`, text: 'odd name\n' },
    { op: 'git', cwd: WT(T1), args: ['add', '-A'] },
    { op: 'git', cwd: WT(T1), args: ['commit', '-qm', 'QA change'] },
    { op: 'change', ws: 'w', taskId: T1 },
    { op: 'verifyCheckout', ws: 'w', taskId: T1, sha: '{tip:noevia/task-' + T1 + '}' },
    { op: 'mergePreflight', ws: 'w', taskId: T1, sha: '{tip:noevia/task-' + T1 + '}' },
    { op: 'release', ws: 'w', taskId: T1 },
    { op: 'change', ws: 'w', taskId: T1 },
    { op: 'change', ws: 'w', taskId: T1, maxFiles: 1 },
    { op: 'change', ws: 'tiny', taskId: T1 },
    { op: 'change', ws: 'w', taskId: T6 },
    { op: 'change', ws: 'w', taskId: 'bad' },
    { op: 'verifyCheckout', ws: 'w', taskId: T1, sha: '{tip:noevia/task-' + T1 + '}' },
    { op: 'verifyCheckout', ws: 'w', taskId: T1, sha: 'abc' },
    { op: 'verifyCheckout', ws: 'w', taskId: T1, sha: '{tip:main}' },
    { op: 'verifyCheckout', ws: 'w', taskId: T6, sha: '{tip:main}' },
    { op: 'branchTip', ws: 'w', taskId: T1 },
    { op: 'branchTip', ws: 'w', taskId: T6 },
    { op: 'branchTip', ws: 'w', taskId: 'bad' },
    { op: 'mergePreflight', ws: 'w', taskId: T1, sha: '{tip:noevia/task-' + T1 + '}' },
    { op: 'git', cwd: 'repos/qa-repo', args: ['checkout', '-q', '--detach'] },
    { op: 'mergePreflight', ws: 'w', taskId: T1, sha: 'abc' },
    { op: 'mergePreflight', ws: 'w', taskId: T1, sha: '{tip:main}' },
    { op: 'mergePreflight', ws: 'w', taskId: 'bad', sha: '{tip:main}' },
    { op: 'mergePreflight', ws: 'w', taskId: T6, sha: '{tip:main}' },
    { op: 'mergePreflight', ws: 'w', taskId: T1, sha: '{tip:noevia/task-' + T1 + '}' },
    { op: 'mergeVerified', ws: 'w', taskId: T1, sha: '{tip:noevia/task-' + T1 + '}' },
    { op: 'mergeVerified', ws: 'w', taskId: T1, sha: '{tip:noevia/task-' + T1 + '}' },
    { op: 'reclaim', ws: 'w', taskId: T1, pinned: ['opencode.json'] },
    { op: 'reclaim', ws: 'w', taskId: T1 },
    { op: 'reclaim', ws: 'w', taskId: T6 },
    { op: 'reclaim', ws: 'w', taskId: 'bad' },
    { op: 'claim', ws: 'w', taskId: T2, repo: 'repos/qa-repo', branch: 'noevia/task-' + T1 },
    { op: 'write', path: `${WT(T1)}/c.txt`, text: 'round two\n' },
    { op: 'git', cwd: WT(T1), args: ['add', 'c.txt'] },
    { op: 'git', cwd: WT(T1), args: ['commit', '-qm', 'QA round two'] },
    { op: 'release', ws: 'w', taskId: T1 },
    { op: 'change', ws: 'w', taskId: T1 },
    // No named base branch: claimed while the source is detached.
    { op: 'claim', ws: 'w', taskId: T3, repo: 'repos/qa-repo' },
    { op: 'release', ws: 'w', taskId: T3 },
    { op: 'change', ws: 'w', taskId: T3 },
    { op: 'mergePreflight', ws: 'w', taskId: T3, sha: '{tip:noevia/task-' + T3 + '}' },
  ],
  'pipeline-clone': [
    { op: 'repo', name: 'qa-repo', files: { 'a.txt': 'a\n', 'readme.TXT': 'plain readme\n' } },
    { op: 'ws', name: 'c', epoch: 'qa-epoch-1', treeRoot: 'trees', owner: 'self' },
    { op: 'clock', t: 1000 },
    { op: 'claim', ws: 'c', taskId: T1, repo: 'repos/qa-repo', branch: 'qa/clone' },
    { op: 'projectSnapshot', ws: 'c', taskId: T1 },
    { op: 'release', ws: 'c', taskId: T1 },
    // A round that never committed starts again from the base.
    { op: 'reclaim', ws: 'c', taskId: T1 },
    { op: 'write', path: `${CL(T1)}/b.txt`, text: 'b\n' },
    { op: 'git', cwd: CL(T1), args: ['add', 'b.txt'] },
    { op: 'git', cwd: CL(T1), args: ['commit', '-qm', 'QA clone commit'] },
    { op: 'release', ws: 'c', taskId: T1 },
    { op: 'reclaim', ws: 'c', taskId: T1, pinned: ['opencode.json'] },
    { op: 'write', path: `${CL(T1)}/c.txt`, text: 'c\n' },
    { op: 'release', ws: 'c', taskId: T1 },
    { op: 'change', ws: 'c', taskId: T1 },
    { op: 'git', cwd: 'repos/qa-repo', args: ['checkout', '-q', '--detach'] },
    { op: 'mergeVerified', ws: 'c', taskId: T1, sha: '{tip:qa/clone}' },
    { op: 'branchTip', ws: 'c', taskId: T1 },
  ],
  // Node's own behaviour where the Rust port differs on purpose (src/lib.rs, "Fixed on purpose").
  'differences': [
    { op: 'repo', name: 'qa-repo', files: { 'a.txt': 'a\n' } },
    { op: 'ws', name: 'w', epoch: 'qa-epoch-1' },
    { op: 'clock', t: 1000 },
    // git refuses a worktree on the branch the source has checked out; the raw error escapes the
    // claim with execFileSync's exit code as its status.
    { op: 'claim', ws: 'w', taskId: T1, repo: 'repos/qa-repo', branch: 'main' },
    { op: 'ws', name: 'c', epoch: 'qa-epoch-1', treeRoot: 'trees', owner: 'self' },
    { op: 'claim', ws: 'c', taskId: T2, repo: 'repos/qa-repo', branch: 'qa/x' },
    // A stray record name makes list() throw inside its own try: it answers [], and a second
    // clone on the same branch is let through.
    { op: 'write', path: 'tenant/code-workspaces/stray.json', text: '{}' },
    { op: 'claim', ws: 'c', taskId: T3, repo: 'repos/qa-repo', branch: 'qa/x' },
    { op: 'list', ws: 'c' },
    // Commit signing names a program; Node's release commit runs it.
    { op: 'claim', ws: 'c', taskId: T4, repo: 'repos/qa-repo', branch: 'qa/sign' },
    { op: 'write', path: 'qa-sign.sh', text: '#!/bin/sh\ntouch <BASE>/pwned\nexit 1\n', mode: 0o755 },
    { op: 'git', cwd: CL(T4), args: ['config', 'commit.gpgsign', 'true'] },
    { op: 'git', cwd: CL(T4), args: ['config', 'gpg.program', '<BASE>/qa-sign.sh'] },
    { op: 'write', path: `${CL(T4)}/a.txt`, text: 'changed\n' },
    { op: 'release', ws: 'c', taskId: T4 },
    { op: 'rm', path: 'pwned' },
    // A filter defined in .git/config.worktree (a file Node's check never reads), named by a
    // .gitattributes only the index holds. Node releases such a clone; the Rust port refuses it.
    { op: 'claim', ws: 'c', taskId: T5, repo: 'repos/qa-repo', branch: 'qa/wtconfig' },
    { op: 'git', cwd: CL(T5), args: ['config', 'core.repositoryformatversion', '1'] },
    { op: 'git', cwd: CL(T5), args: ['config', 'extensions.worktreeConfig', 'true'] },
    { op: 'git', cwd: CL(T5), args: ['config', '--worktree', 'filter.qa.clean', 'sh -c "touch <BASE>/pwned; cat"'] },
    { op: 'write', path: `${CL(T5)}/.gitattributes`, text: '* filter=qa\n' },
    { op: 'git', cwd: CL(T5), args: ['add', '.gitattributes'] },
    { op: 'rm', path: `${CL(T5)}/.gitattributes` },
    { op: 'write', path: `${CL(T5)}/a.txt`, text: 'changed\n' },
    // (The harness's own `git add` above ran the filter as the harness; only noevia's run counts.)
    { op: 'rm', path: 'pwned' },
    { op: 'release', ws: 'c', taskId: T5 },
  ],
};

// ── Snapshot: what is on disk after a step ───────────────────────────────────────────────────
function walk(dir, rel = '') {
  let entries = [];
  try { entries = fs.readdirSync(path.join(dir, rel), { withFileTypes: true }); } catch { return []; }
  const out = [];
  for (const e of entries.sort((a, b) => (a.name < b.name ? -1 : 1))) {
    const r = rel ? `${rel}/${e.name}` : e.name;
    if (e.name === '.git') { out.push(`${r}/`); continue; }
    if (e.isSymbolicLink()) out.push(`${r} -> link`);
    else if (e.isDirectory()) { out.push(`${r}/`); out.push(...walk(dir, r)); }
    else out.push(r);
  }
  return out;
}
const mode = (p) => { try { return (fs.statSync(p).mode & 0o777).toString(8); } catch { return null; } };
function snapshot(base, repos) {
  const root = path.join(base, 'tenant', 'code-workspaces');
  const records = {};
  let names = [];
  try { names = fs.readdirSync(root).sort(); } catch { names = []; }
  for (const n of names) {
    const p = path.join(root, n);
    if (fs.lstatSync(p).isFile()) records[n] = norm(base, fs.readFileSync(p, 'utf8'));
  }
  const git = {};
  for (const r of repos) {
    const repo = path.join(base, 'repos', r);
    git[r] = {
      refs: norm(base, shell(['for-each-ref', '--format=%(refname) %(objectname)'], repo)),
      worktrees: norm(base, shell(['worktree', 'list', '--porcelain'], repo)),
      exclude: (() => { try { return fs.readFileSync(path.join(repo, '.git', 'info', 'exclude'), 'utf8'); } catch { return null; } })(),
    };
  }
  return { records, git, tenant: walk(base, 'tenant'), trees: walk(base, 'trees'), outside: walk(base, 'outside'),
    modes: { root: mode(root), homes: mode(path.join(fs.existsSync(path.join(base, 'trees')) ? path.join(base, 'trees') : root, '.harness-home')) },
    pwned: fs.existsSync(path.join(base, 'pwned')) };
}

// ── Normalising ─────────────────────────────────────────────────────────────────────────────
function norm(base, text) {
  return String(text).split(base).join('<BASE>')
    .replace(/"(uid|gid)":\d+/g, '"$1":"<$1>"')
    .replace(/(Command failed: [^\n]*)\n[\s\S]*$/, '$1\n<stderr>');
}
function outcome(base, fn) {
  try { const ok = fn(); return { ok: ok === undefined ? null : JSON.parse(norm(base, JSON.stringify(ok))) }; }
  catch (e) {
    return { error: { status: e.status ?? null, code: e.code ?? null, link: e.link ?? null, message: norm(base, e.message) } };
  }
}

function runScenario(steps) {
  const base = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), 'qa-code-ws-')));
  const at = (p) => path.join(base, p);
  const repos = [];
  const stores = {};
  let clock = 0, last = null;
  const out = [];
  // `{tip:<branch>}`: that branch's tip in the first source repository, read now.
  const sha = (v) => String(v).replace(/^\{tip:(.+)\}$/, (_, b) => shell(['rev-parse', `refs/heads/${b}`], at(`repos/${repos[0]}`)).trim());
  for (const s of steps) {
    let result;
    switch (s.op) {
      case 'repo': {
        const repo = at(`repos/${s.name}`);
        fs.mkdirSync(repo, { recursive: true });
        // No template: what git init copies differs between git versions.
        shell(['init', '-q', '--template=', '-b', 'main'], repo);
        for (const [name, body] of Object.entries(s.files || {})) fs.writeFileSync(path.join(repo, name), body);
        if (!s.empty) { shell(['add', '.'], repo); shell(['commit', '-qm', 'QA first'], repo); }
        repos.push(s.name);
        break;
      }
      case 'dir': fs.mkdirSync(at(s.path), { recursive: true }); break;
      case 'write': fs.writeFileSync(at(s.path), s.text.split('<BASE>').join(base), s.mode !== undefined ? { mode: s.mode } : {}); if (s.mode !== undefined) fs.chmodSync(at(s.path), s.mode); break;
      case 'rm': fs.rmSync(at(s.path), { force: true }); break;
      case 'symlink': fs.symlinkSync(s.target.split('<BASE>').join(base), at(s.path)); break;
      case 'git': shell(s.args.map((a) => a.split('<BASE>').join(base)), at(s.cwd)); break;
      case 'clock': clock = s.t; break;
      case 'ws': {
        const owner = s.owner === 'self' ? { uid: process.getuid(), gid: process.getgid() } : null;
        stores[s.name] = createCodeWorkspaces({ dir: at('tenant'), treeRoot: s.treeRoot ? at(s.treeRoot) : null, owner, run,
          now: () => clock++, epoch: s.epoch, ...(s.changeMaxBuffer ? { changeMaxBuffer: s.changeMaxBuffer } : {}) });
        break;
      }
      case 'claim': result = outcome(base, () => stores[s.ws].claim({ taskId: s.taskId, repoPath: at(s.repo),
        ...(s.branch !== undefined ? { branch: s.branch } : {}), ...(s.capabilities ? { capabilities: s.capabilities } : {}),
        ...(s.domains ? { domains: s.domains } : {}), ...(s.pinned ? { pinned: s.pinned } : {}) })); break;
      case 'release': result = outcome(base, () => stores[s.ws].release({ taskId: s.taskId, ...(s.removeBranch ? { removeBranch: true } : {}) })); break;
      case 'recover': result = outcome(base, () => stores[s.ws].recover()); break;
      case 'contains': result = outcome(base, () => stores[s.ws].contains(s.taskId, s.candidate.split('<BASE>').join(base))); break;
      case 'headSha': result = outcome(base, () => stores[s.ws].headSha(s.taskId)); break;
      case 'get': result = outcome(base, () => stores[s.ws].get(s.taskId)); break;
      case 'list': result = outcome(base, () => stores[s.ws].list()); break;
      case 'change': result = outcome(base, () => stores[s.ws].change(s.taskId, s.maxFiles !== undefined ? { maxFiles: s.maxFiles } : {})); break;
      case 'reclaim': result = outcome(base, () => stores[s.ws].reclaim({ taskId: s.taskId, ...(s.pinned ? { pinned: s.pinned } : {}) })); break;
      case 'projectSnapshot': result = outcome(base, () => stores[s.ws].projectSnapshot(s.taskId, {
        ...(s.maxFiles !== undefined ? { maxFiles: s.maxFiles } : {}), ...(s.maxReadmeBytes !== undefined ? { maxReadmeBytes: s.maxReadmeBytes } : {}) })); break;
      case 'mergePreflight': result = outcome(base, () => stores[s.ws].mergePreflight(s.taskId, { headSha: sha(s.sha) })); break;
      case 'mergeVerified': result = outcome(base, () => stores[s.ws].mergeVerified(s.taskId, { headSha: sha(s.sha) })); break;
      case 'verifyCheckout': result = outcome(base, () => stores[s.ws].verifyCheckout(s.taskId, sha(s.sha))); break;
      case 'branchTip': result = outcome(base, () => stores[s.ws].branchTip(s.taskId)); break;
      default: throw Error(`unknown op ${s.op}`);
    }
    // A snapshot equal to the previous step's is recorded as "same".
    const now = JSON.stringify(snapshot(base, repos));
    out.push({ ...s, ...(result ? { result } : {}), snapshot: now === last ? 'same' : JSON.parse(now) });
    last = now;
  }
  fs.rmSync(base, { recursive: true, force: true });
  return out;
}

const sha = (n) => crypto.createHash('sha256').update(fs.readFileSync(path.join(serverDir, n))).digest('hex');
const scenarios = Object.fromEntries(Object.entries(SCENARIOS).map(([name, steps]) => [name, runScenario(steps)]));
process.stdout.write(JSON.stringify({ generator: 'crates/code-workspace/tools/gen-fixtures.cjs', sources: { 'code-workspace.cjs': sha('code-workspace.cjs') },
  git: execFileSync('git', ['--version'], { encoding: 'utf8' }).trim(), scenarios }, null, 1) + '\n');
