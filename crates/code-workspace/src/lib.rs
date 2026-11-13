//! noevia-core `server/code-workspace.cjs` in Rust (R7 step 2, part 1): one writer per workspace.
//! A Code task never runs in the registered repository's own tree; it gets a git worktree (or,
//! when the harness runs as another user, a `git clone --shared`) on its own branch, recorded in
//! `<dir>/code-workspaces/<task>.json`: the claim, the release (with the clone's save-back),
//! restart recovery, containment and the live head here; the pipeline's reads (`change`,
//! `projectSnapshot`, `reclaim`, the verified merge, `verifyCheckout`) in [`pipeline`].
//!
//! Records and the trust file are byte-compatible with Node's (`JSON.stringify` of the record in
//! its key order), so either side can read what the other wrote. Every git call is
//! [`git::Git::run`], with Node's environment, deadline and error text.
//!
//! A tree a harness has held is hostile input: git runs commands its `.git/config`, hooks and
//! attributes name. Every git call on such a tree switches those off ([`HOSTILE_OFF`]), a clone
//! whose configuration reaches for them anyway is refused before anything is run on it, and the
//! pinned configuration files (they hold the engine key) are removed without following a link.
//!
//! **Fixed on purpose against Node** (tested in tests/replay.rs):
//! 1. A stray `*.json` in `code-workspaces/` whose name is not a task id is skipped by [`Workspaces::list`];
//!    in Node it made `list()` throw inside its own `try` and return `[]`, which switched off the
//!    one-writer check for every repository.
//! 2. A git failure that escapes a claim (git's own refusal of the worktree or clone) carries no
//!    status (a 500 for the route); Node's error kept execFileSync's exit code as its `status`
//!    (128), which a route then answered with.
//! 3. A record file that is not a JSON object reads as absent.
//! 4. The pinned files are removed through directory file descriptors opened with `O_NOFOLLOW`
//!    (`openat`/`unlinkat`), so a link the harness swaps in after a check is never followed;
//!    Node checked each directory by path and then unlinked by path (sbstndalton/noevia#2527 is
//!    the same race in the pin write).
//!
//! OS error texts (an `lchown` that failed, an unremovable tree) are Rust's, not libuv's.

#![forbid(unsafe_code)]

pub mod git;
pub mod paths;
pub mod pipeline;

use git::{Git, GitError, RunOptions};
use js_json::{wtf16, JValue};
use paths::{dirname, join, realpath, relative, resolve};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;

/// code-workspace.cjs `BRANCH_PREFIX`.
pub const BRANCH_PREFIX: &str = "noevia/task-";
/// Switches off what git would run from a tree's own config, hooks and attributes.
pub const HOSTILE_OFF: [&str; 6] = [
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.attributesFile=/dev/null",
];
const COMMITTER_NAME: &str = "noevia";
const COMMITTER_EMAIL: &str = "noevia@localhost";
/// Harness state that is never the task's work.
const HARNESS_LEAVINGS: [&str; 13] = [
    ".cache/",
    ".config/",
    ".local/",
    ".opencode/",
    ".claude/",
    ".codex/",
    ".qwen/",
    ".pi/",
    ".dsh/",
    "opencode.json",
    "opencode.jsonc",
    ".aider*",
    "node_modules/.cache/",
];

/// A refused operation: Node's `Error` with its `status` (`None`: a 500) and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub status: Option<u16>,
    /// The pipeline reads' stable reason (`no_workspace`, `too_large`, `stale_sha`, …).
    pub code: Option<&'static str>,
    pub message: String,
}

impl Error {
    fn new(status: Option<u16>, message: impl Into<String>) -> Error {
        Error {
            status,
            code: None,
            message: message.into(),
        }
    }
}

fn os(e: &io::Error) -> Error {
    Error::new(None, e.to_string())
}

fn raw(e: GitError) -> Error {
    Error::new(None, e.message)
}

/// A user and group id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ids {
    pub uid: u32,
    pub gid: u32,
}

/// How a task's tree is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A `git worktree` of the repository: the harness runs as noevia.
    Worktree,
    /// A `git clone --shared`: the harness runs as another user and must own its repository.
    Clone,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Worktree => "worktree",
            Mode::Clone => "clone",
        }
    }
}

/// `createCodeWorkspaces` options.
#[derive(Debug, Clone)]
pub struct Options {
    /// The tenant's directory (absolute); records live in `<dir>/code-workspaces`.
    pub dir: String,
    /// Where the trees go when not beside the records (a volume the sandbox mounts at the same path).
    pub tree_root: Option<String>,
    /// Who the harness runs as: each new tree is handed to them.
    pub owner: Option<Ids>,
    /// Who this process runs as (Node's `process.getuid()`), to take a clone back on release.
    pub noevia: Option<Ids>,
    /// `None`: a clone when there is an owner, else a worktree.
    pub mode: Option<Mode>,
    /// This process: claims of another epoch are interrupted by [`Workspaces::recover`].
    pub epoch: String,
    /// The output cap for a diff read for review (`None`: [`pipeline::CHANGE_MAX_BUFFER`]).
    pub change_max_buffer: Option<usize>,
}

/// What [`Workspaces::claim`] takes.
#[derive(Debug, Clone, Default)]
pub struct Claim<'a> {
    pub task_id: &'a str,
    pub repo_path: &'a str,
    pub branch: Option<&'a str>,
    pub capabilities: &'a [String],
    pub domains: &'a [String],
    /// The harness's pinned config paths, as given (anything not a plain relative path is dropped).
    pub pinned: &'a [JValue],
}

type Clock = Box<dyn Fn() -> f64 + Send + Sync>;
type Chown = Box<dyn Fn(&Path, Ids) -> io::Result<()> + Send + Sync>;

/// One tenant's task workspaces.
pub struct Workspaces {
    root: String,
    trees: String,
    trust_file: String,
    owner: Option<Ids>,
    noevia: Option<Ids>,
    mode: Mode,
    epoch: String,
    now: Clock,
    git: Git,
    chown: Chown,
    change_max_buffer: usize,
    lock: Mutex<()>,
}

/// `/^[0-9a-f-]{36}$/`.
pub fn valid_task_id(id: &str) -> bool {
    id.len() == 36
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
}

fn branch_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-'))
        && !name.contains("..")
        && !name.starts_with('-')
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_string()).collect()
}

fn hostile(list: &[&str]) -> Vec<String> {
    HOSTILE_OFF
        .iter()
        .chain(list.iter())
        .map(|s| (*s).to_string())
        .collect()
}

fn s(text: &str) -> JValue {
    JValue::Str(text.to_string())
}

fn opt_str(v: Option<String>) -> JValue {
    v.map_or(JValue::Null, JValue::Str)
}

fn ids_value(ids: Option<Ids>) -> JValue {
    ids.map_or(JValue::Null, |i| {
        JValue::obj([
            ("uid", JValue::Num(f64::from(i.uid))),
            ("gid", JValue::Num(f64::from(i.gid))),
        ])
    })
}

fn ids_of(v: &JValue) -> Option<Ids> {
    let uid = v.get("uid").as_num()?;
    let gid = v.get("gid").as_num()?;
    let ok = |n: f64| n >= 0.0 && n <= f64::from(u32::MAX) && n.fract() == 0.0;
    (ok(uid) && ok(gid)).then_some(Ids {
        uid: uid as u32,
        gid: gid as u32,
    })
}

/// `{...record, k: v, ...}`: an existing key keeps its place, a new one goes at the end.
fn with(record: &JValue, pairs: Vec<(&str, JValue)>) -> JValue {
    let mut items = match record {
        JValue::Obj(items) => items.clone(),
        _ => Vec::new(),
    };
    for (k, v) in pairs {
        wtf16::set(&mut items, k, v);
    }
    JValue::Obj(items)
}

fn text_of(v: &JValue) -> String {
    js_json::to_js_string(v).unwrap_or_default()
}

/// The first line of `text`.
fn first_line(text: &str) -> &str {
    text.split('\n').next().unwrap_or("")
}

/// `gitReason`: what git actually refused.
fn git_reason(error: &GitError) -> String {
    let text = if error.stderr.is_empty() {
        error.message.as_str()
    } else {
        error.stderr.as_str()
    };
    let lower = text.to_lowercase();
    if lower.contains("dubious ownership") {
        return "That repository is owned by another user and git refused to read it.".into();
    }
    if lower.contains("not a git repository") {
        return "Not a git repository".into();
    }
    let first = js_slice(first_line(text), 200);
    let first = if first.is_empty() {
        "unknown error"
    } else {
        first.as_str()
    };
    format!("git could not read that repository: {first}")
}

/// `s.slice(0, n)` in UTF-16 units.
fn js_slice(text: &str, n: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in text.chars() {
        let w = c.len_utf16();
        if units + w > n {
            if units < n {
                // JS keeps the high surrogate of a cut pair; held as U+FFFD here.
                out.push('\u{FFFD}');
            }
            break;
        }
        units += w;
        out.push(c);
    }
    out
}

/// `pinnedPaths`: relative, plain paths only.
pub fn pinned_paths(list: &[JValue]) -> Vec<String> {
    list.iter()
        .filter_map(|v| js_json::to_js_string(v).ok())
        .filter(|p| {
            !p.is_empty()
                && !p.starts_with('/')
                && !p.split(['/', '\\']).any(|part| part == "..")
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-'))
        })
        .collect()
}

fn pins_of(record: &JValue) -> Vec<String> {
    match record.get("pinned") {
        JValue::Arr(items) => pinned_paths(items),
        _ => Vec::new(),
    }
}

/// A key a released clone's `.git/config` must not set (code-workspace.cjs `HOSTILE_KEYS`).
fn hostile_key(k: &str) -> bool {
    let rest_ends = |prefix: &str, suffix: &str| {
        k.strip_prefix(prefix)
            .is_some_and(|rest| rest.ends_with(suffix))
    };
    k.starts_with("filter.")
        || rest_ends("diff.", ".command")
        || rest_ends("diff.", ".textconv")
        || rest_ends("merge.", ".driver")
        || matches!(
            k,
            "core.fsmonitor"
                | "core.hookspath"
                | "core.sshcommand"
                | "core.gitproxy"
                | "core.worktree"
                | "core.askpass"
                | "core.editor"
                | "core.pager"
        )
        || k.strip_prefix("credential.")
            .is_some_and(|rest| rest == "helper" || rest.ends_with(".helper"))
        || k.starts_with("alias.")
        || k.starts_with("include.")
        || k.starts_with("includeif.")
        || k.starts_with("uploadpack.")
        || k.starts_with("receive.")
        || k.starts_with("sendemail.")
}

/// JS `\s`.
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// `/(^|\s)filter=/m`.
fn names_filter(body: &str) -> bool {
    body.match_indices("filter=").any(|(i, _)| {
        i == 0
            || body
                .get(..i)
                .and_then(|head| head.chars().next_back())
                .is_some_and(js_space)
    })
}

/// Directory entries in byte order (Node's readdirSync: libuv sorts them).
fn sorted_entries(dir: &str) -> io::Result<Vec<(String, fs::FileType)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        out.push((name, entry.file_type()?));
    }
    out.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    Ok(out)
}

fn mkdirs(dir: &str, mode: u32) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    builder.create(dir)
}

fn write_private(path: &str, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// `fs.rmSync(target, { recursive: true, force: true })`: never follows a link.
pub fn remove_all(target: &str) -> io::Result<()> {
    match fs::symlink_metadata(target) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(target),
        Ok(_) => fs::remove_file(target),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// code-workspace.cjs `defaultChown`: every entry under `target`, links themselves not followed.
pub fn chown_tree(target: &Path, ids: Ids) -> io::Result<()> {
    let mut stack = vec![target.to_path_buf()];
    while let Some(entry) = stack.pop() {
        #[cfg(unix)]
        std::os::unix::fs::lchown(&entry, Some(ids.uid), Some(ids.gid))?;
        let Ok(meta) = fs::symlink_metadata(&entry) else {
            continue;
        };
        if meta.is_dir() {
            for child in fs::read_dir(&entry)? {
                stack.push(child?.path());
            }
        }
    }
    Ok(())
}

/// A pinned file could not be removed. `link`: a link sat on the way to it.
struct PinError {
    message: String,
    link: bool,
}

impl Workspaces {
    /// The workspaces with the system clock, `git` from PATH and a recursive `lchown`.
    pub fn new(options: Options) -> Workspaces {
        Workspaces::with(
            options,
            Box::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |d| d.as_millis() as f64)
            }),
            Git::default(),
            Box::new(chown_tree),
        )
    }

    pub fn with(options: Options, now: Clock, git: Git, chown: Chown) -> Workspaces {
        let dir = if options.dir.starts_with('/') {
            options.dir.clone()
        } else {
            let cwd = std::env::current_dir()
                .ok()
                .and_then(|p| p.into_os_string().into_string().ok())
                .unwrap_or_else(|| "/".into());
            resolve(&cwd, &options.dir)
        };
        let root = join(&dir, "code-workspaces");
        let trees = options.tree_root.clone().unwrap_or_else(|| root.clone());
        Workspaces {
            trust_file: join(&root, "trusted-repositories.gitconfig"),
            root,
            trees,
            owner: options.owner,
            noevia: options.noevia,
            mode: options.mode.unwrap_or(if options.owner.is_some() {
                Mode::Clone
            } else {
                Mode::Worktree
            }),
            epoch: options.epoch,
            now,
            git,
            chown,
            change_max_buffer: options
                .change_max_buffer
                .unwrap_or(pipeline::CHANGE_MAX_BUFFER),
            lock: Mutex::new(()),
        }
    }

    /// Where the records live.
    pub fn root(&self) -> &str {
        &self.root
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn env(&self) -> RunOptions {
        RunOptions {
            env: vec![
                ("GIT_CONFIG_GLOBAL".into(), self.trust_file.clone()),
                ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
            ],
            ..RunOptions::default()
        }
    }

    fn run(&self, args: &[String], cwd: Option<&str>) -> Result<String, GitError> {
        self.git.run(args, cwd.map(Path::new), &self.env())
    }

    fn check_id(task_id: &str) -> Result<(), Error> {
        if valid_task_id(task_id) {
            Ok(())
        } else {
            Err(Error::new(Some(400), "Invalid task id"))
        }
    }

    fn record_file(&self, task_id: &str) -> String {
        join(&self.root, &format!("{task_id}.json"))
    }

    /// The record, or `None` (no file, or not a JSON object).
    fn read(&self, task_id: &str) -> Result<Option<JValue>, Error> {
        Self::check_id(task_id)?;
        let Ok(text) = fs::read_to_string(self.record_file(task_id)) else {
            return Ok(None);
        };
        Ok(wtf16::parse(&text).ok().filter(JValue::is_object))
    }

    fn write(&self, record: JValue) -> Result<JValue, Error> {
        mkdirs(&self.root, 0o700).map_err(|e| os(&e))?;
        let id = record.get("taskId").as_str().unwrap_or("").to_string();
        Self::check_id(&id)?;
        let text = wtf16::stringify(&record).unwrap_or_default();
        write_private(&self.record_file(&id), text.as_bytes()).map_err(|e| os(&e))?;
        Ok(record)
    }

    /// Every record, newest claim first.
    pub fn list(&self) -> Vec<JValue> {
        let Ok(entries) = sorted_entries(&self.root) else {
            return Vec::new();
        };
        let mut out: Vec<JValue> = entries
            .iter()
            .filter_map(|(name, _)| name.strip_suffix(".json"))
            .filter(|id| valid_task_id(id))
            .filter_map(|id| self.read(id).ok().flatten())
            .collect();
        let at = |r: &JValue| match r.get("claimedAt").as_num() {
            Some(n) if !n.is_nan() => n,
            _ => f64::NEG_INFINITY,
        };
        out.sort_by(|a, b| at(b).total_cmp(&at(a)));
        out
    }

    /// The record of a task (Node's `get`), [`JValue::Null`] when there is none.
    pub fn get(&self, task_id: &str) -> Result<JValue, Error> {
        Ok(self.read(task_id)?.unwrap_or(JValue::Null))
    }

    fn trust(&self, paths: &[&str]) -> io::Result<()> {
        mkdirs(&self.root, 0o700)?;
        let current = fs::read_to_string(&self.trust_file).unwrap_or_default();
        let lines: Vec<String> = paths
            .iter()
            .filter(|p| !p.is_empty())
            .map(|p| format!("\tdirectory = {p}\n"))
            .filter(|l| !current.contains(l.as_str()))
            .collect();
        if lines.is_empty() {
            return Ok(());
        }
        let head = if current.is_empty() {
            "[safe]\n".to_string()
        } else {
            current
        };
        write_private(
            &self.trust_file,
            format!("{head}{}", lines.concat()).as_bytes(),
        )
    }

    /// Takes the workspace for one task (one live claim per repository and branch).
    pub fn claim(&self, claim: Claim<'_>) -> Result<JValue, Error> {
        let _held = self.guard();
        let id = claim.task_id;
        Self::check_id(id)?;
        if self.read(id)?.is_some() {
            return Err(Error::new(Some(409), "This task already has a workspace"));
        }
        let repo =
            realpath(claim.repo_path).ok_or_else(|| Error::new(Some(400), "No such repository"))?;
        self.trust(&[&repo, &join(&repo, ".git")])
            .map_err(|e| os(&e))?;
        let git_dir = self
            .run(&args(&["rev-parse", "--absolute-git-dir"]), Some(&repo))
            .map_err(|e| Error::new(Some(400), git_reason(&e)))?;
        self.trust(&[&git_dir]).map_err(|e| {
            let as_git = GitError {
                message: e.to_string(),
                stderr: String::new(),
                too_large: false,
            };
            Error::new(Some(400), git_reason(&as_git))
        })?;
        let name = match claim.branch {
            Some(b) => b.to_string(),
            None => format!("{BRANCH_PREFIX}{id}"),
        };
        if !branch_name_ok(&name) {
            return Err(Error::new(Some(400), "Invalid branch name"));
        }
        if let Some(held) = self.list().into_iter().find(|r| {
            r.get("status").as_str() != Some("released")
                && r.get("repo").as_str() == Some(repo.as_str())
                && r.get("branch").as_str() == Some(name.as_str())
        }) {
            return Err(Error::new(
                Some(409),
                format!(
                    "Another task already writes {name} in this repository ({})",
                    text_of(held.get("status"))
                ),
            ));
        }
        let tree = join(&self.trees, id);
        let home = join(&self.trees, &format!(".harness-home/{id}"));
        self.make_dirs(&home).map_err(|e| os(&e))?;
        match self.mode {
            Mode::Clone => {
                self.run(&args(&["clone", "--quiet", "--shared", &repo, &tree]), None)
                    .map_err(raw)?;
                self.run(&args(&["checkout", "--quiet", "-B", &name]), Some(&tree))
                    .map_err(raw)?;
            }
            Mode::Worktree => {
                self.run(&args(&["worktree", "add", "-B", &name, &tree]), Some(&repo))
                    .map_err(raw)?;
            }
        }
        let base_sha = self
            .run(&hostile(&["rev-parse", "HEAD"]), Some(&tree))
            .ok()
            .filter(|s| !s.is_empty());
        let base_branch = self
            .run(
                &hostile(&["symbolic-ref", "--quiet", "--short", "HEAD"]),
                Some(&repo),
            )
            .ok()
            .filter(|s| !s.is_empty());
        let pins = self.prepare_tree(&repo, &tree, &home, claim.pinned)?;
        let path = realpath(&tree).ok_or_else(|| {
            Error::new(
                None,
                format!("ENOENT: no such file or directory, realpath '{tree}'"),
            )
        })?;
        let strings = |list: &[String]| JValue::Arr(list.iter().map(|x| s(x)).collect());
        let mut record = vec![
            ("taskId", s(id)),
            ("repo", s(&repo)),
            ("branch", s(&name)),
            ("path", s(&path)),
            ("home", s(&home)),
            ("status", s("held")),
            ("mode", s(self.mode.name())),
            ("baseSha", opt_str(base_sha)),
        ];
        if let Some(b) = base_branch.filter(|b| branch_name_ok(b)) {
            record.push(("baseBranch", JValue::Str(b)));
        }
        record.extend([
            ("owner", ids_value(self.owner)),
            ("noevia", ids_value(self.noevia)),
            ("capabilities", strings(claim.capabilities)),
            ("domains", strings(claim.domains)),
            ("pinned", strings(&pins)),
            ("epoch", s(&self.epoch)),
            ("claimedAt", JValue::Num((self.now)())),
        ]);
        self.write(JValue::obj(record))
    }

    fn make_dirs(&self, home: &str) -> io::Result<()> {
        mkdirs(&self.root, 0o700)?;
        if self.trees != self.root {
            mkdirs(&self.trees, 0o755)?;
        }
        // The shared parent must be traversable by the harness user; only the task's own
        // directory in it is private. An existing one left too strict is corrected.
        let parent = dirname(home);
        mkdirs(&parent, 0o755)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&parent, fs::Permissions::from_mode(0o755));
        }
        mkdirs(home, 0o700)
    }

    /// Takes a half-made workspace away again.
    fn undo(&self, repo: &str, tree: &str, home: &str) {
        match self.mode {
            Mode::Clone => {
                let _ = remove_all(tree);
            }
            Mode::Worktree => {
                if self
                    .run(
                        &hostile(&["worktree", "remove", "--force", tree]),
                        Some(repo),
                    )
                    .is_err()
                {
                    let _ = remove_all(tree);
                }
                let _ = self.run(&hostile(&["worktree", "prune"]), Some(repo));
            }
        }
        let _ = remove_all(home);
    }

    /// What a new tree needs before a harness may have it: the pinned paths refused when the
    /// repository tracks one, the harness's leavings excluded from git, the hand-over.
    fn prepare_tree(
        &self,
        repo: &str,
        tree: &str,
        home: &str,
        pinned: &[JValue],
    ) -> Result<Vec<String>, Error> {
        let pins = pinned_paths(pinned);
        if !pins.is_empty() {
            let mut list = hostile(&["ls-files", "-z", "--"]);
            list.extend(pins.iter().cloned());
            let tracked = match self.run(&list, Some(tree)) {
                Ok(t) => t,
                Err(e) => {
                    self.undo(repo, tree, home);
                    return Err(Error::new(Some(400), git_reason(&e)));
                }
            };
            let names: Vec<&str> = tracked.split('\0').filter(|n| !n.is_empty()).collect();
            if !names.is_empty() {
                self.undo(repo, tree, home);
                let shown: Vec<&str> = names.iter().take(3).copied().collect();
                return Err(Error::new(
                    Some(409),
                    format!(
                        "This repository tracks {}, which noevia writes the harness configuration (and its key) into. Remove it from the repository to run this harness.",
                        shown.join(", ")
                    ),
                ));
            }
        }
        if let Err(why) = self.exclude(tree, &pins) {
            self.undo(repo, tree, home);
            return Err(Error::new(
                Some(500),
                format!(
                    "Could not keep the harness configuration out of git: {}",
                    first_line(&why)
                ),
            ));
        }
        if let Some(owner) = self.owner {
            let handed = (self.chown)(Path::new(tree), owner)
                .and_then(|()| (self.chown)(Path::new(home), owner));
            if let Err(e) = handed {
                self.undo(repo, tree, home);
                return Err(Error::new(
                    Some(500),
                    format!("Could not hand the workspace to the harness user: {e}"),
                ));
            }
        }
        Ok(pins)
    }

    /// Adds the harness's leavings and the pins to the exclude file git uses for `tree`.
    fn exclude(&self, tree: &str, pins: &[String]) -> Result<(), String> {
        let where_ = self
            .run(
                &hostile(&["rev-parse", "--git-path", "info/exclude"]),
                Some(tree),
            )
            .map_err(|e| e.message)?;
        if where_.is_empty() {
            return Err("git did not say where its exclude file is".into());
        }
        let file = resolve(tree, &where_);
        let lines: Vec<String> = HARNESS_LEAVINGS
            .iter()
            .map(|l| (*l).to_string())
            .chain(pins.iter().map(|p| format!("/{p}")))
            .collect();
        let current = fs::read_to_string(&file).unwrap_or_default();
        let have: Vec<&str> = current.split('\n').collect();
        let missing: Vec<&str> = lines
            .iter()
            .map(String::as_str)
            .filter(|l| !have.contains(l))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        mkdirs(&dirname(&file), 0o777).map_err(|e| e.to_string())?;
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&file)
            .and_then(|mut f| {
                f.write_all(
                    format!(
                        "\n# noevia: harness state, not the task's work\n{}\n",
                        missing.join("\n")
                    )
                    .as_bytes(),
                )
            })
            .map_err(|e| e.to_string())
    }

    /// Deletes the pinned config files from a tree, never following a link on the way down: each
    /// directory is opened relative to its parent with `O_NOFOLLOW`, and the file is unlinked
    /// relative to its own directory, so a link swapped in after a check leads nowhere (Node
    /// checked by path and then unlinked by path).
    fn drop_pinned(&self, record: &JValue) -> Result<(), PinError> {
        use rustix::fs::{open, openat, statat, unlinkat, AtFlags, FileType, Mode, OFlags};
        use rustix::io::Errno;
        let base = resolve("/", record.get("path").as_str().unwrap_or(""));
        let kind = |st: &rustix::fs::Stat| FileType::from_raw_mode(st.st_mode);
        let link = |dir: &str| PinError {
            message: format!("{} is a symbolic link", relative(&base, dir)),
            link: true,
        };
        let dflags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        'pins: for rel in pins_of(record) {
            let target = join(&base, &rel);
            if !target.starts_with(&format!("{base}/")) {
                continue;
            }
            let rel_path = relative(&base, &target);
            let parts: Vec<&str> = rel_path.split('/').collect();
            let Some((last, dirs)) = parts.split_last() else {
                continue;
            };
            // The tree itself is the record's own realpath, opened as Node joined onto it.
            let Ok(mut fd) = open(base.as_str(), dflags, Mode::empty()) else {
                continue;
            };
            let mut dir = base.clone();
            for part in dirs {
                dir = join(&dir, part);
                let Ok(st) = statat(&fd, *part, AtFlags::SYMLINK_NOFOLLOW) else {
                    continue 'pins;
                };
                match kind(&st) {
                    FileType::Symlink => return Err(link(&dir)),
                    // A file where a directory should be: nothing can sit under it.
                    FileType::Directory => {}
                    _ => continue 'pins,
                }
                fd = match openat(&fd, *part, dflags | OFlags::NOFOLLOW, Mode::empty()) {
                    Ok(next) => next,
                    // Swapped for a link (or a file) since the check.
                    Err(Errno::LOOP | Errno::NOTDIR) => return Err(link(&dir)),
                    Err(_) => continue 'pins,
                };
            }
            let Ok(st) = statat(&fd, *last, AtFlags::SYMLINK_NOFOLLOW) else {
                continue;
            };
            if kind(&st) == FileType::Directory {
                return Err(PinError {
                    message: format!("{rel} is a directory"),
                    link: false,
                });
            }
            unlinkat(&fd, *last, AtFlags::empty()).map_err(|e| PinError {
                message: io::Error::from(e).to_string(),
                link: false,
            })?;
        }
        Ok(())
    }

    /// The live head of a held tree noevia controls; `None` for an owner-held tree (never read
    /// live: its `.git` is the harness's), a tree with no commits, or no held claim.
    pub fn head_sha(&self, task_id: &str) -> Result<Option<String>, Error> {
        let _held = self.guard();
        let Some(record) = self.read(task_id)? else {
            return Ok(None);
        };
        if record.get("status").as_str() != Some("held") || record.get("owner").truthy() {
            return Ok(None);
        }
        let path = record.get("path").as_str().unwrap_or("").to_string();
        Ok(self
            .run(&hostile(&["rev-parse", "HEAD"]), Some(&path))
            .ok()
            .filter(|s| !s.is_empty()))
    }

    /// Whether `candidate` is inside the task's tree, judged with realpath (a path about to be
    /// created by its nearest existing ancestor). `None` when the task holds no workspace.
    pub fn contains(&self, task_id: &str, candidate: &str) -> Result<Option<bool>, Error> {
        let _held = self.guard();
        let Some(record) = self.read(task_id)? else {
            return Ok(None);
        };
        if record.get("status").as_str() != Some("held") {
            return Ok(None);
        }
        if candidate.is_empty() {
            return Ok(Some(false));
        }
        let root_path = record.get("path").as_str().unwrap_or("");
        let base = resolve("/", root_path);
        let target = resolve(&base, candidate);
        let Some(resolved_root) = realpath(root_path) else {
            return Ok(Some(false));
        };
        // A link whose target is missing would be judged by its parent, not where a write lands.
        if fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink())
            && fs::metadata(&target).is_err()
        {
            return Ok(Some(false));
        }
        let mut probe = target.clone();
        let mut unresolved = 0;
        let probe = loop {
            if let Some(real) = realpath(&probe) {
                break real;
            }
            let parent = dirname(&probe);
            unresolved += 1;
            if parent == probe || unresolved > 64 {
                return Ok(Some(false));
            }
            probe = parent;
        };
        if relative(&probe, &target).split('/').any(|p| p == "..") {
            return Ok(Some(false));
        }
        let rel = relative(&resolved_root, &probe);
        Ok(Some(
            rel.is_empty() || (!rel.starts_with("..") && !rel.starts_with('/')),
        ))
    }

    /// The branch's tip in the SOURCE repository (never the task's tree).
    fn branch_head(&self, record: &JValue) -> JValue {
        let repo = record.get("repo").as_str().unwrap_or("").to_string();
        let branch = record.get("branch").as_str().unwrap_or("").to_string();
        opt_str(
            self.run(&hostile(&["rev-parse", "--verify", &branch]), Some(&repo))
                .ok()
                .filter(|s| !s.is_empty()),
        )
    }

    fn stuck(&self, record: &JValue, error: String) -> Result<JValue, Error> {
        let at = JValue::Num((self.now)());
        let head = self.branch_head(record);
        self.write(with(
            record,
            vec![
                ("status", s("stuck")),
                ("releasedAt", at),
                ("headSha", head),
                ("error", JValue::Str(error)),
            ],
        ))
    }

    /// Why a released clone must not be touched by git, or `None` when it looks inert.
    fn hostile_reason(&self, tree: &str) -> Option<String> {
        let git_dir = join(tree, ".git");
        let Ok(meta) = fs::symlink_metadata(&git_dir) else {
            return Some("the clone has no .git directory".into());
        };
        if !meta.is_dir() {
            return Some("the clone\u{2019}s .git is not a plain directory".into());
        }
        let config = join(&git_dir, "config");
        let listing = match self.run(
            &args(&["config", "--file", &config, "--name-only", "--list"]),
            Some(tree),
        ) {
            Ok(l) => l,
            Err(e) => {
                return Some(format!(
                    "its .git/config could not be read: {}",
                    first_line(&e.message)
                ))
            }
        };
        let mut bad: Vec<String> = Vec::new();
        for key in listing.split('\n') {
            let key = git::js_trim(key).to_lowercase();
            if !key.is_empty() && hostile_key(&key) && !bad.contains(&key) {
                bad.push(key);
            }
        }
        if !bad.is_empty() {
            let shown: Vec<&str> = bad.iter().take(5).map(String::as_str).collect();
            return Some(format!(
                "its .git/config sets {}, which would make git run a command",
                shown.join(", ")
            ));
        }
        let hooks = join(&git_dir, "hooks");
        for (name, _) in sorted_entries(&hooks).unwrap_or_default() {
            if name.ends_with(".sample") {
                continue;
            }
            if let Ok(m) = fs::metadata(join(&hooks, &name)) {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if m.is_file() && m.permissions().mode() & 0o111 != 0 {
                        return Some(format!(".git/hooks/{name} is an executable hook"));
                    }
                }
                #[cfg(not(unix))]
                let _ = m;
            }
        }
        let mut attr_files = vec![join(&git_dir, "info/attributes")];
        walk_attributes(tree, 0, &mut attr_files);
        for file in attr_files {
            let Ok(bytes) = fs::read(&file) else {
                continue;
            };
            if names_filter(&String::from_utf8_lossy(&bytes)) {
                return Some(format!("{} names a filter driver", relative(tree, &file)));
            }
        }
        None
    }

    /// Gives the workspace back; the branch survives unless `remove_branch`. A clone's work is
    /// committed (as noevia) and fetched into the source first; anything that fails keeps the
    /// tree and marks the claim stuck. [`JValue::Null`] when the task has no record.
    pub fn release(&self, task_id: &str, remove_branch: bool) -> Result<JValue, Error> {
        let _held = self.guard();
        let Some(record) = self.read(task_id)? else {
            return Ok(JValue::Null);
        };
        if record.get("status").as_str() == Some("released") {
            return Ok(record);
        }
        let path = record.get("path").as_str().unwrap_or("").to_string();
        let repo = record.get("repo").as_str().unwrap_or("").to_string();
        let branch = record.get("branch").as_str().unwrap_or("").to_string();
        let mut removed = true;
        let mut error = JValue::Null;
        let clone = record.get("mode").as_str() == Some("clone");
        if clone {
            if let (true, Some(noevia)) =
                (record.get("owner").truthy(), ids_of(record.get("noevia")))
            {
                if let Err(e) = (self.chown)(Path::new(&path), noevia) {
                    return self.stuck(
                        &record,
                        format!("Could not take the workspace back from the harness user: {e}"),
                    );
                }
            }
            if let Some(why) = self.hostile_reason(&path) {
                return self.stuck(&record, format!("Refused to release the workspace: {why}. It was left in place for inspection."));
            }
            if let Err(e) = self.drop_pinned(&record) {
                return self.stuck(
                    &record,
                    format!(
                        "Could not remove the harness configuration before saving: {}",
                        e.message
                    ),
                );
            }
            let saved = (|| -> Result<(), GitError> {
                if !self
                    .run(&hostile(&["status", "--porcelain"]), Some(&path))?
                    .is_empty()
                {
                    self.run(&hostile(&["add", "--all"]), Some(&path))?;
                    let name = format!("user.name={COMMITTER_NAME}");
                    let email = format!("user.email={COMMITTER_EMAIL}");
                    let message = format!(
                        "noevia: work in progress from task {}\n\nCommitted by noevia when the task ended, because the harness left it uncommitted.",
                        text_of(record.get("taskId"))
                    );
                    self.run(
                        &hostile(&[
                            "-c",
                            &name,
                            "-c",
                            &email,
                            "commit",
                            "--quiet",
                            "--no-verify",
                            "-m",
                            &message,
                        ]),
                        Some(&path),
                    )?;
                }
                Ok(())
            })();
            if let Err(e) = saved {
                return self.stuck(
                    &record,
                    format!(
                        "Could not save the task\u{2019}s uncommitted changes: {}",
                        e.message
                    ),
                );
            }
            let refspec = format!("{branch}:{branch}");
            if let Err(e) = self.run(
                &hostile(&["fetch", "--quiet", &path, &refspec]),
                Some(&repo),
            ) {
                let lower = e.message.to_lowercase();
                if !(lower.contains("couldn't find remote ref")
                    || lower.contains("not found in upstream"))
                {
                    return self.stuck(
                        &record,
                        format!("Could not save the task\u{2019}s branch: {}", e.message),
                    );
                }
            }
            if let Err(e) = remove_all(&path) {
                removed = false;
                error = JValue::Str(e.to_string());
            }
            if record.get("home").truthy() {
                let _ = remove_all(record.get("home").as_str().unwrap_or(""));
            }
        } else {
            if let Err(e) = self.drop_pinned(&record) {
                if e.link {
                    return self.stuck(&record, format!("Refused to release the workspace: {}. It was left in place for inspection.", e.message));
                }
            }
            if let Err(e) = self.run(
                &hostile(&["worktree", "remove", "--force", &path]),
                Some(&repo),
            ) {
                removed = false;
                error = JValue::Str(e.message);
            }
            let _ = self.run(&hostile(&["worktree", "prune"]), Some(&repo));
        }
        if removed && remove_branch {
            let _ = self.run(&args(&["branch", "-D", &branch]), Some(&repo));
        }
        let at = JValue::Num((self.now)());
        let head = self.branch_head(&record);
        self.write(with(
            &record,
            vec![
                ("status", s(if removed { "released" } else { "stuck" })),
                ("releasedAt", at),
                ("headSha", head),
                ("error", error),
            ],
        ))
    }

    /// After a restart, held claims of another epoch are interrupted: reported, never reused, the
    /// tree kept; only the pinned config (it holds the engine key) is removed.
    pub fn recover(&self) -> Result<Vec<JValue>, Error> {
        let _held = self.guard();
        let mut out = Vec::new();
        for record in self.list() {
            if record.get("status").as_str() != Some("held")
                || record.get("epoch").as_str() == Some(self.epoch.as_str())
            {
                continue;
            }
            let at = JValue::Num((self.now)());
            let mut interrupted = with(
                &record,
                vec![("status", s("interrupted")), ("interruptedAt", at)],
            );
            if let (true, Some(noevia)) = (
                interrupted.get("owner").truthy(),
                ids_of(interrupted.get("noevia")),
            ) {
                let path = interrupted.get("path").as_str().unwrap_or("").to_string();
                let _ = (self.chown)(Path::new(&path), noevia);
            }
            interrupted = match self.drop_pinned(&interrupted) {
                Ok(()) => with(&interrupted, vec![("pinnedDropped", JValue::Bool(true))]),
                Err(e) => with(
                    &interrupted,
                    vec![
                        ("pinnedDropped", JValue::Bool(false)),
                        ("pinnedDropError", JValue::Str(e.message)),
                    ],
                ),
            };
            out.push(self.write(interrupted)?);
        }
        Ok(out)
    }
}

/// Every `.gitattributes` under `dir`, at most 64 levels down, `.git` skipped, links not followed.
fn walk_attributes(dir: &str, depth: usize, out: &mut Vec<String>) {
    if depth > 64 {
        return;
    }
    let Ok(entries) = sorted_entries(dir) else {
        return;
    };
    for (name, kind) in entries {
        if name == ".git" {
            continue;
        }
        let full = join(dir, &name);
        if kind.is_dir() {
            walk_attributes(&full, depth + 1, out);
        } else if name == ".gitattributes" {
            out.push(full);
        }
    }
}
