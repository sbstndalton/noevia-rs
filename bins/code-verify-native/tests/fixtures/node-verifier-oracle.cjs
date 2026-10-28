'use strict';
// The verifier: server-measured tests for Code tasks (#703, part of #511).
//
// Same image as the coding sandbox, but its OWN container (`code-verify` in
// deploy/examples/code-sandbox.override.yml), and that separation is the first boundary: the agent
// runs as uid 1000 with a writable /tmp, HOME and task repositories, and nothing it leaves behind
// may reach the run that decides whether its work passes. Inside the verifier there are three
// identities, and the test code never shares one with anything that matters:
//
//   * the SERVER runs as root, but with every capability dropped except CAP_SETUID/CAP_SETGID (and
//     no-new-privileges). It owns the socket and its directory (root, 0770), never runs git and never
//     runs repository code. Without CAP_DAC_OVERRIDE / CAP_FOWNER / CAP_KILL it is no stronger than
//     an ordinary user over anyone else's files or processes — it can only drop to the two uids below;
//   * GIT (the copy) runs as VERIFY_GIT_UID (1002), with the repositories' shared gid as its group;
//   * the TEST COMMAND runs as VERIFY_TEST_UID (1003). It cannot signal the server (another uid, no
//     capabilities), cannot touch the socket or its directory (root, 0770), and cannot chmod, write
//     or replace the copy (owned by 1002, read-only). Its HOME and TMPDIR are its own.
//
// The second boundary is time: the verifier is ONE-SHOT. It serves exactly one request and exits;
// Docker restarts it with fresh tmpfs, and every process left in the container (a test that escaped
// its process group with `setsid`, a git helper) dies with PID 1. Before the copy is used, and after
// the test, the processes of the uid that just ran are killed as well (`kill -9 -1` run AS that uid,
// which needs no capability), so a survivor cannot even outlive its phase.
//
// What runs, and on what:
//   * The command is the operator's alone: `CODE_VERIFY=name|command` (one per line) in this
//     container's environment. noevia sends a repository name, the source path, the commit and a
//     nonce — never a command — and nothing in the repository decides what runs.
//   * The copy is real and verified: the source's `.git/config` is COPIED first and that copy is
//     checked for anything git would run; then `git clone --no-local` (index-pack recomputes every
//     object id) into the git uid's own tmpfs, `git fsck`, and HEAD and its tree must equal the
//     requested commit. The clone runs with GIT_CONFIG_NOSYSTEM, a global config noevia wrote, a HOME
//     the git uid cannot write, and `-c`/`--config` overrides. Note that the local transport runs
//     upload-pack INSIDE the source repository, which reads the source's live config — the copy-and-
//     check cannot freeze that. git itself ignores the dangerous upload-pack keys from repository
//     config (`uploadpack.packObjectsHook`), and anything else a swapped config could make it run
//     runs as the git uid, in a one-shot container with no network, and is killed before the copy is
//     used; what it could do to the copy is what fsck and the HEAD/tree checks are for.
//   * The test gets a fixed environment: PATH, its own HOME/TMPDIR, LANG, CI, NO_COLOR. No proxy
//     variables, nothing noevia sent. Stdin closed, no core dumps, a CPU limit per process, a wall
//     limit (CODE_VERIFY_WALL_MS, default 10 min) covering the copy and the run, its process group
//     killed at the end.
//   * Output is data: a rolling tail of CODE_VERIFY_TAIL_BYTES (default 16 KiB) inside ONE JSON
//     result line, with the request's nonce echoed, so nothing a test prints can forge a result.
const net = require('node:net'), fs = require('node:fs'), path = require('node:path'), os = require('node:os');
const { spawn } = require('node:child_process');

const MAX_START_LINE = 64 * 1024;
const DEFAULT_WALL_MS = 10 * 60 * 1000;
const DEFAULT_TAIL_BYTES = 16 * 1024;
const MAX_TAIL_BYTES = 256 * 1024;
const SCRATCH_PREFIX = 'noevia-verify-';
const REPO_NAME = /^[A-Za-z0-9_.-]{1,64}$/;
const COMMIT = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/;
const NONCE = /^[0-9a-f]{32,128}$/;
const positive = (value, fallback) => { const n = Number(value); return Number.isFinite(n) && n > 0 ? Math.floor(n) : fallback; };

const GIT_OFF = ['-c', 'core.fsmonitor=false', '-c', 'core.hooksPath=/dev/null', '-c', 'core.attributesFile=/dev/null',
  '-c', 'protocol.file.allow=always', '-c', 'advice.detachedHead=false'];
const CLONE_CONFIG = ['--config', 'core.hooksPath=/dev/null', '--config', 'core.fsmonitor=false', '--config', 'core.attributesFile=/dev/null'];
const HOSTILE_KEYS = [
  /^filter\./, /^diff\..*\.(command|textconv)$/, /^merge\..*\.driver$/, /^core\.fsmonitor$/, /^core\.hookspath$/,
  /^core\.sshcommand$/, /^core\.gitproxy$/, /^core\.worktree$/, /^core\.askpass$/, /^core\.editor$/, /^core\.pager$/,
  /^core\.alternaterefscommand$/, /^credential\.(.*\.)?helper$/, /^alias\./, /^include\./, /^includeif\./,
  /^uploadpack\./, /^receive\./, /^sendemail\./, /^pack\./, /^protocol\./, /^transfer\./,
];

/** `CODE_VERIFY=name|command` per line. First entry for a name wins; malformed lines are dropped. */
function parseVerify(raw) {
  const out = new Map();
  for (const line of String(raw || '').split(/\r?\n/)) {
    const bar = line.indexOf('|');
    if (bar < 1) continue;
    const name = line.slice(0, bar).trim(), command = line.slice(bar + 1).trim();
    if (!REPO_NAME.test(name) || !command || command.length > 4096 || out.has(name)) continue;
    out.set(name, command);
  }
  return out;
}

function insideRoot(root, candidate) {
  let resolvedRoot, resolved;
  try { resolvedRoot = fs.realpathSync(root); } catch { return null; }
  try { resolved = fs.realpathSync(String(candidate || '')); } catch { return null; }
  const rel = path.relative(resolvedRoot, resolved);
  if (rel !== '' && (rel.startsWith('..') || path.isAbsolute(rel))) return null;
  return resolved;
}

/**
 * Whether a resolved path can be written into the git config the verifier generates (#855). The
 * path comes from the shared workspaces volume, which the agent can write: a repository swapped
 * for a symlink to a directory named `x\n[uploadpack]\n\tpackObjectsHook=…` would otherwise add
 * its own section to that config. Control characters (a newline ends the value), `"` and `\`
 * (quoting and escapes) are refused; everything else is safe inside a quoted value. Called on
 * the realpath, since that is what is written.
 */
function configSafePath(p) {
  return typeof p === 'string' && p.length > 0 && !/[\u0000-\u001f\u007f-\u009f"\\]/.test(p);
}

/** The only environment a verify command gets. Nothing from noevia, no proxy. */
function verifyEnv(home, tmp, basePath = process.env.PATH) {
  return { PATH: basePath || '/usr/local/bin:/usr/bin:/bin', HOME: home, TMPDIR: tmp, LANG: 'C.UTF-8', CI: 'true', NO_COLOR: '1' };
}

/** A rolling buffer that keeps only the last `cap` bytes of everything written to it. */
function createTail(cap) {
  let chunks = [], kept = 0, total = 0;
  return {
    push(chunk) {
      const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(String(chunk));
      total += buf.length;
      chunks.push(buf); kept += buf.length;
      if (kept > cap * 2) { const all = Buffer.concat(chunks); chunks = [all.subarray(all.length - cap)]; kept = cap; }
    },
    read() {
      const all = Buffer.concat(chunks);
      const tail = all.length > cap ? all.subarray(all.length - cap) : all;
      return { tail: tail.toString('utf8'), tailBytes: tail.length, totalBytes: total, truncated: total > tail.length };
    },
  };
}

/** `{uid, gid}` from `uid:gid` or `uid`, or null. */
function parseIdentity(raw) {
  const m = String(raw || '').trim().match(/^(\d+)(?::(\d+))?$/);
  return m ? { uid: Number(m[1]), gid: Number(m[2] ?? m[1]) } : null;
}

/**
 * @param {{root: string, copyRoot?: string, scratchRoot?: string, ctlRoot?: string,
 *          verify?: Map<string, string>|string, wallMs?: number, tailBytes?: number, graceMs?: number,
 *          shell?: string, verifyPath?: string, git?: string, gitUser?: {uid: number, gid: number}|null,
 *          testUser?: {uid: number, gid: number}|null, oneShot?: boolean, onDone?: () => void,
 *          log?: (line: string) => void, spawnFn?: Function, kill?: (pid: number, signal: string) => void}} deps
 *
 * `root`: where the read-only source repositories are mounted. `copyRoot`: where git (as `gitUser`)
 * makes the copies; `scratchRoot`: where the test (as `testUser`) gets its HOME and TMPDIR; `ctlRoot`:
 * the server's own files (the git config it writes). With `gitUser`/`testUser` null everything runs
 * as the current user — that is how the unprivileged tests run it; production refuses that (see main).
 */
function createVerifier({ root, copyRoot = os.tmpdir(), scratchRoot = copyRoot, ctlRoot = copyRoot,
  verify = new Map(), wallMs = DEFAULT_WALL_MS, tailBytes = DEFAULT_TAIL_BYTES, graceMs = 5000, shell = '/bin/sh',
  verifyPath = process.env.PATH, git = 'git', gitUser = null, testUser = null, oneShot = false, onDone = () => {},
  log = () => {}, spawnFn = spawn, kill = process.kill.bind(process) }) {
  const commands = verify instanceof Map ? verify : parseVerify(verify);
  const wall = positive(wallMs, DEFAULT_WALL_MS);
  const tailCap = Math.min(positive(tailBytes, DEFAULT_TAIL_BYTES), MAX_TAIL_BYTES);
  const self = typeof process.getuid === 'function' ? process.getuid() : null;
  let running = 0, served = 0;
  const sockets = new Set();
  const PATHV = verifyPath || '/usr/local/bin:/usr/bin:/bin';

  /** Run a program as `who` (or as ourselves). Resolves {code, signal, stdout, stderr}. */
  // cwd defaults to `/`: a dropped uid may not be able to enter the server's own working directory.
  function exec(file, args, { who = null, cwd = '/', env = { PATH: PATHV }, input = null, timeoutMs = 60_000, signal = null } = {}) {
    return new Promise((resolve) => {
      let child;
      try {
        child = spawnFn(file, args, { cwd, env, stdio: [input == null ? 'ignore' : 'pipe', 'pipe', 'pipe'],
          ...(who ? { uid: who.uid, gid: who.gid } : {}) });
      } catch (e) { resolve({ code: null, signal: null, stdout: '', stderr: e.message }); return; }
      let stdout = '', stderr = '';
      child.stdout?.on('data', (d) => { if (stdout.length < 1024 * 1024) stdout += d; });
      child.stderr?.on('data', (d) => { if (stderr.length < 64 * 1024) stderr += d; });
      const timer = setTimeout(() => { try { child.kill('SIGKILL'); } catch { /* gone */ } }, Math.max(1, timeoutMs));
      const onAbort = () => { try { child.kill('SIGKILL'); } catch { /* gone */ } };
      signal?.addEventListener?.('abort', onAbort, { once: true });
      if (input != null) child.stdin?.end(input);
      child.on('error', (e) => { stderr += e.message; });
      child.on('close', (code, sig) => { clearTimeout(timer); signal?.removeEventListener?.('abort', onAbort); resolve({ code, signal: sig, stdout, stderr }); });
    });
  }
  /**
   * Kill every process of a uid we dropped to, from a process OF that uid: kill(-1) needs no
   * capability for one's own uid and never signals the caller. Only for a real other uid — never
   * for our own (that would kill the verifier and everything else this user runs).
   */
  async function reap(who) {
    if (!who || who.uid === self || who.uid === 0) return;
    await exec(shell, ['-c', 'kill -9 -1 2>/dev/null; exit 0'], { who, timeoutMs: 10_000 });
  }
  async function removeAs(who, target, why) {
    const r = await exec(shell, ['-c', 'chmod -R u+w "$1" 2>/dev/null; rm -rf "$1"', 'rm', target], { who, timeoutMs: 60_000 });
    if (r.code !== 0 || fs.existsSync(target)) log(`could not remove ${why} ${target}: ${String(r.stderr).split('\n')[0] || `exit ${r.code}`}`);
  }

  /** Leftovers from a run the process did not live to clean up (a restart normally wipes them). */
  async function sweep() {
    let swept = 0;
    /** @type {Array<{dir: string, who: {uid: number, gid: number}|null}>} */
    const places = [{ dir: copyRoot, who: gitUser }, { dir: scratchRoot, who: testUser }, { dir: ctlRoot, who: null }];
    for (const { dir, who } of places) {
      let names = [];
      try { names = fs.readdirSync(dir); } catch (e) {
        // A 0700 directory owned by the dropped uid (the tmpfs at /verify/run) is not ours to list, but
        // that uid can. Only a listing that fails for it too is a real failure worth a log line.
        if (who && (e.code === 'EACCES' || e.code === 'EPERM')) {
          const r = await exec(shell, ['-c', 'ls -A "$1"', 'ls', dir], { who, timeoutMs: 10_000 });
          if (r.code === 0) names = String(r.stdout).split('\n').filter(Boolean);
          else { log(`could not read ${dir} to sweep it: ${String(r.stderr).split('\n')[0] || `exit ${r.code}`}`); continue; }
        } else { log(`could not read ${dir} to sweep it: ${e.message}`); continue; }
      }
      for (const name of names.filter((n) => n.startsWith(SCRATCH_PREFIX))) { swept++; await removeAs(who, path.join(dir, name), 'leftover verify directory'); }
    }
    return swept;
  }

  const server = net.createServer((socket) => {
    sockets.add(socket);
    socket.on('error', () => {});
    socket.setEncoding('utf8');
    if (oneShot && served > 0) { socket.destroy(); return; }
    served++;
    // One request per process: no second connection is accepted while, or after, this one runs.
    if (oneShot) server.close();
    let buffer = '', started = false, closed = false, nonce = null;
    const answer = (result) => { if (!socket.destroyed) socket.end(JSON.stringify({ noevia: 'verify-result', nonce, ...result }) + '\n'); };
    const refuse = (error, message) => { log(`refused: ${message}`); answer({ ok: false, error, message }); };
    let abort = () => {};
    let work = Promise.resolve();
    socket.on('close', () => {
      closed = true; sockets.delete(socket); abort();
      if (oneShot) work.finally(() => onDone());
    });
    socket.on('data', (chunk) => {
      if (started) return;   // one request per connection; nothing after it is read
      buffer += chunk;
      if (buffer.length > MAX_START_LINE) { started = true; return refuse('bad_request', 'request line too long'); }
      const end = buffer.indexOf('\n');
      if (end === -1) return;
      started = true;
      const line = buffer.slice(0, end);
      buffer = '';
      let msg;
      try { msg = JSON.parse(line); } catch { return refuse('bad_request', 'the first line must be a verify request'); }
      if (!msg || msg.noevia !== 'verify') return refuse('bad_request', 'the first line must be a verify request');
      nonce = typeof msg.nonce === 'string' && NONCE.test(msg.nonce) ? msg.nonce : null;
      if (!nonce) return refuse('bad_request', 'verify needs a nonce');
      const name = typeof msg.repo === 'string' ? msg.repo : '';
      const sha = typeof msg.headSha === 'string' ? msg.headSha : '';
      if (!REPO_NAME.test(name)) return refuse('bad_request', 'verify needs a repository name');
      if (!COMMIT.test(sha)) return refuse('bad_request', 'verify needs a full commit id');
      const command = commands.get(name);
      if (!command) return refuse('not_configured', `no verification command is configured for ${name} (CODE_VERIFY)`);
      const src = insideRoot(root, msg.source);
      if (!src) return refuse('outside', 'that source repository is not under the verifier’s read-only root');
      if (!configSafePath(src)) return refuse('hostile_source', 'the source path holds characters that cannot go into a git config');
      if (running > 0) return refuse('busy', 'the verifier is already running a verification; try again when it finishes');
      running++;
      // The answer goes out only after the run's processes are reaped and its directories removed.
      work = run({ name, sha, command, src })
        .then((result) => { running--; if (result) answer(result); },
          (e) => { running--; log(`verify crashed: ${e.message}`); refuse('internal', 'the verifier failed'); });
    });

    async function run({ name, sha, command, src }) {
      const began = Date.now();
      const deadline = began + wall;
      const left = () => Math.max(1, deadline - Date.now());
      const id = `${SCRATCH_PREFIX}${process.pid}-${began}-${Math.random().toString(16).slice(2, 10)}`;
      const ctl = path.join(ctlRoot, id);          // the server's: git config, the source config copy
      const copy = path.join(copyRoot, id);        // git's: the clone
      const scratch = path.join(scratchRoot, id);  // the test's: HOME and TMPDIR
      const tree = path.join(copy, 'tree');
      // Checked again before anything is created: the git config below is the only place the path
      // is written, and nothing may reach it unvalidated (#855).
      if (!configSafePath(src)) return { ok: false, error: 'hostile_source', message: 'the source path holds characters that cannot go into a git config' };
      fs.mkdirSync(ctl, { mode: 0o755 });
      // safe.directory is honoured only from a global/system FILE (not -c): the source belongs to
      // another uid. Exactly this source, nothing else.
      const gitConfig = path.join(ctl, 'gitconfig');
      // Quoted, so `#` or `;` in a path stays part of the value instead of starting a comment.
      fs.writeFileSync(gitConfig, `[safe]\n\tdirectory = "${src}"\n\tdirectory = "${path.join(src, '.git')}"\n`, { mode: 0o644 });
      const gitEnv = { PATH: PATHV, HOME: ctl, LANG: 'C.UTF-8', GIT_CONFIG_GLOBAL: gitConfig, GIT_CONFIG_NOSYSTEM: '1',
        GIT_TERMINAL_PROMPT: '0', GIT_ASKPASS: '/bin/false' };
      const controller = new AbortController();
      let pid = null, stopped = false, timedOut = false;
      const stopGroup = () => {
        if (!pid || stopped) return;
        stopped = true;
        // Without CAP_KILL the server cannot signal another uid's processes; then the reaper,
        // running AS the test uid, is what stops them (a setsid escapee included).
        try { kill(-pid, 'SIGTERM'); } catch { /* gone, or not ours to signal */ }
        reap(testUser).catch(() => {});
        const t = setTimeout(() => { try { kill(-pid, 'SIGKILL'); } catch { /* gone */ } }, graceMs); t.unref?.();
      };
      abort = () => { controller.abort(); stopGroup(); };
      const cleanup = async () => {
        await reap(testUser); await reap(gitUser);
        await removeAs(testUser, scratch, 'verify scratch');
        await removeAs(gitUser, copy, 'verify copy');
        await removeAs(null, ctl, 'verify control directory');
      };
      const finish = async (result) => { await cleanup(); return closed ? null : result; };
      const gitRun = async (args, cwd) => {
        const r = await exec(git, [...GIT_OFF, ...args], { who: gitUser, cwd, env: gitEnv, timeoutMs: left(), signal: controller.signal });
        if (r.code !== 0) throw Object.assign(Error(`git ${args[0]} failed`), { detail: String(r.stderr).trim().split('\n')[0].slice(0, 200), timeout: Date.now() >= deadline });
        return String(r.stdout).trim();
      };

      // 1. The copy, verified. The source's config is copied first and only the copy is read.
      try {
        const configFile = path.join(src, '.git', 'config');
        let st;
        try { st = fs.lstatSync(path.join(src, '.git')); } catch { return finish({ ok: false, error: 'hostile_source', message: 'the source has no .git directory' }); }
        if (!st.isDirectory()) return finish({ ok: false, error: 'hostile_source', message: 'the source’s .git is not a plain directory' });
        let configText = '';
        try { configText = fs.readFileSync(configFile, 'utf8'); } catch { /* no config is fine */ }
        const configCopy = path.join(ctl, 'source-config');
        fs.writeFileSync(configCopy, configText, { mode: 0o644 });
        const listing = await exec(git, ['config', '--file', configCopy, '--name-only', '--list'], { who: gitUser, env: gitEnv, timeoutMs: 10_000 });
        if (listing.code !== 0) log(`could not list the source config: ${String(listing.stderr).split('\n')[0]}`);
        if (listing.code !== 0) return finish({ ok: false, error: 'hostile_source', message: 'the source’s .git/config could not be read' });
        const bad = String(listing.stdout).split('\n').map((k) => k.trim().toLowerCase()).filter((k) => k && HOSTILE_KEYS.some((re) => re.test(k)));
        if (bad.length) return finish({ ok: false, error: 'hostile_source', message: `the source’s .git/config sets ${[...new Set(bad)].slice(0, 5).join(', ')}` });
        await gitRun(['clone', '--quiet', '--no-local', '--no-hardlinks', '--no-checkout', ...CLONE_CONFIG, src, tree]);
        // Nothing git (or anything a source config made it start) left running may outlive the
        // copy step: it would own the copy the tests are about to trust.
        await reap(gitUser);
        await gitRun(['fsck', '--no-dangling', '--no-progress'], tree);
        const commit = await gitRun(['rev-parse', '--verify', '--end-of-options', `${sha}^{commit}`], tree);
        if (commit !== sha) return finish({ ok: false, error: 'head_mismatch', message: 'the source does not hold that commit' });
        await gitRun(['checkout', '--quiet', '--detach', sha], tree);
        const head = await gitRun(['rev-parse', 'HEAD'], tree);
        const treeId = await gitRun(['rev-parse', 'HEAD^{tree}'], tree);
        const expectedTree = await gitRun(['rev-parse', `${sha}^{tree}`], tree);
        if (head !== sha || treeId !== expectedTree) return finish({ ok: false, error: 'head_mismatch', message: 'the copy did not land on the requested commit' });
        // Read-only, and owned by the git uid: the test uid can neither write it nor chmod it back.
        const ro = await exec(shell, ['-c', 'chmod -R a-w "$1" && chmod 0555 "$2"', 'ro', tree, copy], { who: gitUser, timeoutMs: left() });
        if (ro.code !== 0) throw Object.assign(Error('could not make the copy read-only'), { detail: String(ro.stderr).split('\n')[0] });
      } catch (e) {
        if (closed) return finish({});
        const message = e.timeout ? 'ran out of time preparing the copy' : `could not make a verified copy of that commit${e.detail ? `: ${e.detail}` : ''}`;
        log(`copy failed for ${name} at ${sha.slice(0, 12)}: ${e.detail || e.message}`);
        return finish({ ok: false, error: 'checkout', message });
      }
      if (closed) return finish({});

      // 2. The operator's command, as the test uid, in the copy.
      const cpuSeconds = Math.max(1, Math.ceil(wall / 1000));
      const home = path.join(scratch, 'home'), tmp = path.join(scratch, 'tmp');
      // HOME/TMPDIR are created by the test uid itself (the server cannot write its tmpfs), and the
      // operator's command is handed to the shell as "$1", never spliced into the script.
      const script = `umask 077; mkdir -p "$HOME" "$TMPDIR" || exit 125; ulimit -c 0 2>/dev/null; ulimit -t ${cpuSeconds} 2>/dev/null; exec ${shell} -c "$1"`;
      const tail = createTail(tailCap);
      log(`verify of ${name} at ${sha.slice(0, 12)} started`);
      let result = null;
      await new Promise((resolve) => {
        let exit = null, done = false, timer = null;
        const end = () => {
          if (done) return;
          done = true;
          if (timer) clearTimeout(timer);
          stopGroup();   // its background children too
          const { tail: text, tailBytes, totalBytes, truncated } = tail.read();
          log(`verify of ${name} at ${sha.slice(0, 12)} ended (${timedOut ? 'timed out' : exit?.signal || exit?.code})`);
          result = { ok: true, repo: name, headSha: sha, exitCode: exit && Number.isInteger(exit.code) ? exit.code : null,
            signal: exit?.signal || null, timedOut, durationMs: Date.now() - began, tail: text, tailBytes, totalBytes, truncated };
          resolve();
        };
        let child;
        try {
          child = spawnFn(shell, ['-c', script, 'noevia-verify', command], { cwd: tree, env: verifyEnv(home, tmp, verifyPath),
            stdio: ['ignore', 'pipe', 'pipe'], detached: true, ...(testUser ? { uid: testUser.uid, gid: testUser.gid } : {}) });
        } catch (e) { log(`could not start: ${e.message}`); result = { ok: false, error: 'spawn', message: 'could not start the verification' }; done = true; resolve(); return; }
        pid = child.pid || null;
        timer = setTimeout(() => {
          timedOut = true;
          log(`verify ran past its ${wall} ms limit; stopping it`);
          stopGroup();
          const t = setTimeout(end, graceMs + 50); t.unref?.();
        }, left());
        timer.unref?.();
        child.stdout?.on('data', (out) => tail.push(out));
        child.stderr?.on('data', (out) => tail.push(out));
        child.on('error', (error) => { log(`verify failed to run: ${error.message}`); exit = exit || { code: null, signal: null }; end(); });
        // 'close' waits for the pipes; a background child holding them must not hold the answer back.
        child.on('exit', (code, sig) => {
          exit = { code, signal: sig };
          const t = setTimeout(end, Math.min(graceMs, 500)); t.unref?.();
          child.on('close', end);
        });
      });
      return finish(result);
    }
  });

  return {
    server, sweep, running: () => running,
    /** A unix socket path (production: VERIFY_SOCKET) or a TCP port (tests). */
    listen: async (target, host = '127.0.0.1') => {
      await sweep();
      return new Promise((resolve, reject) => {
        server.once('error', reject);
        if (typeof target === 'string') {
          try { fs.unlinkSync(target); } catch (e) { if (e.code !== 'ENOENT') log(`could not remove the old socket: ${e.message}`); }
          server.listen(target, () => { try { fs.chmodSync(target, 0o660); } catch (e) { log(`could not chmod the socket: ${e.message}`); } resolve(target); });
        } else server.listen(target, host, () => resolve(server.address()));
      });
    },
    close: () => new Promise((r) => { for (const s of sockets) s.destroy(); if (!server.listening) { r(); return; } server.close(() => r()); }),
  };
}

module.exports = { createVerifier, parseVerify, parseIdentity, verifyEnv, createTail, insideRoot, configSafePath,
  HOSTILE_KEYS, DEFAULT_WALL_MS, DEFAULT_TAIL_BYTES, SCRATCH_PREFIX };

if (require.main === module) {
  const env = process.env;
  const root = env.VERIFY_SOURCE_ROOT, socketPath = env.VERIFY_SOCKET;
  const gitUser = parseIdentity(env.VERIFY_GIT_USER), testUser = parseIdentity(env.VERIFY_TEST_USER);
  const self = process.getuid?.();
  // Refuse a deployment where the test could share a uid with the server or with git.
  const problem = !root || !socketPath ? 'VERIFY_SOURCE_ROOT and VERIFY_SOCKET are required'
    : !gitUser || !testUser ? 'VERIFY_GIT_USER and VERIFY_TEST_USER are required (uid:gid)'
      : self !== 0 ? 'the verifier server must start as root (with only CAP_SETUID/CAP_SETGID) so it can drop to the git and test uids'
        : testUser.uid === 0 || gitUser.uid === 0 || testUser.uid === gitUser.uid ? 'the git and test uids must be distinct and not root'
          : null;
  if (problem) { console.error(`code-verify: ${problem}`); process.exit(2); }
  const started = Date.now();
  // Docker's restart back-off resets after ten seconds of uptime; staying up that long keeps
  // back-to-back verifications from waiting minutes. The listener is already closed by then.
  const minUptimeMs = positive(env.VERIFY_MIN_UPTIME_MS, 11_000);
  const verifier = createVerifier({ root, verify: parseVerify(env.CODE_VERIFY), gitUser, testUser, oneShot: true,
    copyRoot: env.VERIFY_COPY_ROOT || '/verify/copy', scratchRoot: env.VERIFY_SCRATCH_ROOT || '/verify/run',
    ctlRoot: env.VERIFY_CTL_ROOT || os.tmpdir(),
    wallMs: positive(env.CODE_VERIFY_WALL_MS, DEFAULT_WALL_MS),
    tailBytes: positive(env.CODE_VERIFY_TAIL_BYTES, DEFAULT_TAIL_BYTES),
    onDone: () => {
      const wait = Math.max(0, minUptimeMs - (Date.now() - started));
      console.log(`[code-verify] served its one request; exiting${wait ? ` in ${wait} ms` : ''}`);
      setTimeout(() => process.exit(0), wait);
    },
    log: (line) => console.log(`[code-verify] ${line}`) });
  verifier.listen(socketPath).then(() => {
    console.log(`[code-verify] listening on ${socketPath} (one request), sources under ${root} (read-only), ${parseVerify(env.CODE_VERIFY).size} repositories configured`);
  }, (e) => { console.error(`code-verify: ${e.message}`); process.exit(1); });
}
