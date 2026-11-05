//! Workspace records stay in the existing tenant format. All operations run in the owning
//! actor's blocking worker; agent-controlled trees are never passed to Git without hardening.
use crate::{facts::CodeConfig, git};
mod pipeline;
use js_json::{wtf16, JValue};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

// Node performed these transactions synchronously. Keep trust/exclude read-modify-writes
// serialized across tenant actors as well; no asynchronous work runs under this lock.
static TRANSACTION: std::sync::Mutex<()> = std::sync::Mutex::new(());

const LEAVINGS: &[&str] = &[
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
#[derive(Debug)]
pub enum Error {
    Coded { code: &'static str, message: String },
    Unavailable,
    Busy,
    Refused { status: u16, message: String },
    Io(std::io::Error),
    Git(git::Error),
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<git::Error> for Error {
    fn from(e: git::Error) -> Self {
        Self::Git(e)
    }
}
fn refused(status: u16, message: impl Into<String>) -> Error {
    Error::Refused {
        status,
        message: message.into(),
    }
}
fn valid_id(id: &str) -> bool {
    id.len() == 36
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c) || c == b'-')
}
pub fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.contains("..")
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_./-".contains(&c))
}
pub fn full_sha(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64)
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub fn pinned_paths(values: &[String]) -> Vec<String> {
    values
        .iter()
        .filter(|p| {
            !p.is_empty()
                && !Path::new(p.as_str()).is_absolute()
                && !p.split(['/', '\\']).any(|part| part == "..")
                && p.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_./-".contains(&c))
        })
        .cloned()
        .collect()
}
fn string(value: impl Into<String>) -> JValue {
    JValue::Str(value.into())
}
fn object(fields: Vec<(&str, JValue)>) -> JValue {
    JValue::Obj(fields.into_iter().map(|(k, v)| (k.into(), v)).collect())
}
fn plain_dir(path: &Path) -> Result<(), Error> {
    if !fs::symlink_metadata(path)?.file_type().is_dir() {
        return Err(Error::Unavailable);
    }
    Ok(())
}
fn mkdir(path: &Path, mode: u32) -> Result<(), Error> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(mode);
    builder.create(path)?;
    plain_dir(path)
}
fn open(path: &Path, write: bool) -> Result<fs::File, Error> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    if write {
        options.write(true).create(true).truncate(false).mode(0o600);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::Unavailable);
    }
    Ok(file)
}
/// The lease fences separate native front incarnations. It deliberately never authorizes a
/// legacy writer; release pairing must retire that writer before this store becomes reachable.
pub struct Store {
    root: PathBuf,
    trees: PathBuf,
    owner: Option<(u32, u32)>,
    epoch: String,
    lease: fs::File,
}
impl Drop for Store {
    fn drop(&mut self) {
        self.retire();
    }
}
impl Store {
    pub(crate) fn retire(&self) {
        let _ = self.lease.unlock();
    }
    pub fn open(
        data_dir: &Path,
        user: &str,
        config: &CodeConfig,
        epoch: String,
    ) -> Result<Self, Error> {
        let user_dir =
            server_projects::store::user_dir(data_dir, user).map_err(|_| Error::Unavailable)?;
        plain_dir(&data_dir.join("users"))?;
        plain_dir(&user_dir)?;
        let root = user_dir.join("code-workspaces");
        mkdir(&root, 0o700)?;
        let lease = open(&root.join("native-owner.lock"), true)?;
        lease.try_lock().map_err(|_| Error::Busy)?;
        let trees = config.tree_root.clone().unwrap_or_else(|| root.clone());
        Ok(Self {
            root,
            trees,
            owner: config.owner,
            epoch,
            lease,
        })
    }
    fn file(&self, id: &str) -> Result<PathBuf, Error> {
        if !valid_id(id) {
            return Err(refused(400, "Invalid task id"));
        }
        Ok(self.root.join(format!("{id}.json")))
    }
    pub fn read(&self, id: &str) -> Result<Option<JValue>, Error> {
        let path = self.file(id)?;
        self.assert_owner()?;
        let mut file = match open(&path, false) {
            Ok(file) => file,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if file.metadata()?.len() > 1024 * 1024 {
            return Err(Error::Unavailable);
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(std::str::from_utf8(&bytes)
            .ok()
            .and_then(|s| wtf16::parse(s).ok()))
    }
    fn write(&self, record: &JValue) -> Result<(), Error> {
        self.assert_owner()?;
        plain_dir(self.root.parent().ok_or(Error::Unavailable)?)?;
        plain_dir(&self.root)?;
        let path = self.file(record.get("taskId").as_str().ok_or(Error::Unavailable)?)?;
        let encoded = wtf16::stringify(record).ok_or(Error::Unavailable)?;
        let mut file = open(&path, true)?;
        file.set_len(0)?;
        file.write_all(encoded.as_bytes())?;
        Ok(())
    }
    pub fn list(&self) -> Result<Vec<JValue>, Error> {
        self.assert_owner()?;
        plain_dir(&self.root)?;
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if let Some(id) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.strip_suffix(".json"))
                .filter(|id| valid_id(id))
            {
                if let Ok(Some(record)) = self.read(id) {
                    records.push(record);
                }
            }
        }
        Ok(records)
    }
    fn assert_owner(&self) -> Result<(), Error> {
        use std::os::unix::fs::MetadataExt;
        let held = self.lease.metadata()?;
        let named = fs::symlink_metadata(self.root.join("native-owner.lock"))
            .map_err(|_| Error::Unavailable)?;
        if !named.file_type().is_file() || held.dev() != named.dev() || held.ino() != named.ino() {
            return Err(Error::Unavailable);
        }
        plain_dir(self.root.parent().ok_or(Error::Unavailable)?)?;
        plain_dir(&self.root)
    }
    fn environment(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "GIT_CONFIG_GLOBAL".into(),
                self.root
                    .join("trusted-repositories.gitconfig")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
        ])
    }
    fn trust(&self, paths: &[PathBuf]) -> Result<(), Error> {
        let path = self.root.join("trusted-repositories.gitconfig");
        let mut file = open(&path, true)?;
        let mut current = String::new();
        file.read_to_string(&mut current)?;
        if current.is_empty() {
            current.push_str("[safe]\n");
        }
        for path in paths {
            let line = format!("\tdirectory = {}\n", path.display());
            if !current.contains(&line) {
                current.push_str(&line);
            }
        }
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(current.as_bytes())?;
        Ok(())
    }
    fn run(&self, cwd: &Path, args: &[&str], cancelled: &AtomicBool) -> Result<String, Error> {
        let args = args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        Ok(git::run(
            &args,
            cwd,
            &self.environment(),
            1024 * 1024,
            false,
            cancelled,
        )?)
    }
    fn safe(&self, cwd: &Path, args: &[&str], cancelled: &AtomicBool) -> Result<String, Error> {
        let mut all = git::HOSTILE_OFF.to_vec();
        all.extend_from_slice(args);
        self.run(cwd, &all, cancelled)
    }
}

impl Store {
    #[allow(clippy::too_many_arguments)]
    pub fn claim(
        &self,
        id: &str,
        repo: &Path,
        branch: Option<&str>,
        capabilities: &[String],
        domains: &[String],
        pinned: &[String],
        now: i64,
        cancelled: &AtomicBool,
    ) -> Result<JValue, Error> {
        use std::os::unix::fs::PermissionsExt;
        let _transaction = TRANSACTION.lock().map_err(|_| Error::Unavailable)?;
        self.file(id)?;
        if self.read(id)?.is_some() {
            return Err(refused(409, "This task already has a workspace"));
        }
        let repo = fs::canonicalize(repo).map_err(|_| refused(400, "No such repository"))?;
        self.trust(&[repo.clone(), repo.join(".git")])?;
        let git_dir = self
            .run(&repo, &["rev-parse", "--absolute-git-dir"], cancelled)
            .map_err(git_reason)?;
        self.trust(&[PathBuf::from(git_dir)])?;
        let name = branch
            .filter(|b| !b.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("noevia/task-{id}"));
        if !valid_branch(&name) {
            return Err(refused(400, "Invalid branch name"));
        }
        for record in self.list()? {
            if record.get("status").as_str() != Some("released")
                && record.get("repo").as_str() == repo.to_str()
                && record.get("branch").as_str() == Some(name.as_str())
            {
                return Err(refused(
                    409,
                    format!(
                        "Another task already writes {name} in this repository ({})",
                        record.get("status").as_str().unwrap_or("undefined")
                    ),
                ));
            }
        }
        let tree = self.trees.join(id);
        let home = self.trees.join(".harness-home").join(id);
        if fs::symlink_metadata(&tree).is_ok() {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "workspace path already exists",
            )));
        }
        mkdir(
            &self.trees,
            if self.trees == self.root {
                0o700
            } else {
                0o755
            },
        )?;
        let home_parent = home.parent().ok_or(Error::Unavailable)?;
        mkdir(home_parent, 0o755)?;
        let _ = fs::set_permissions(home_parent, fs::Permissions::from_mode(0o755));
        mkdir(&home, 0o700)?;
        let clone = self.owner.is_some();
        let mut undo = Undo {
            store: self,
            repo: &repo,
            tree: &tree,
            home: &home,
            clone,
            armed: true,
        };
        let tree_text = tree.to_str().ok_or(Error::Unavailable)?;
        let repo_text = repo.to_str().ok_or(Error::Unavailable)?;
        if clone {
            self.run(
                &std::env::current_dir()?,
                &["clone", "--quiet", "--shared", repo_text, tree_text],
                cancelled,
            )?;
            self.run(&tree, &["checkout", "--quiet", "-B", &name], cancelled)?;
        } else {
            self.run(
                &repo,
                &["worktree", "add", "-B", &name, tree_text],
                cancelled,
            )?;
        }
        let base_sha = self
            .safe(&tree, &["rev-parse", "HEAD"], cancelled)
            .ok()
            .filter(|s| !s.is_empty());
        let base_branch = self
            .safe(
                &repo,
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
                cancelled,
            )
            .ok()
            .filter(|s| valid_branch(s));
        let pins = self.prepare_tree(&tree, &home, pinned, cancelled)?;
        let paths =
            |values: &[String]| JValue::Arr(values.iter().map(|v| string(v.clone())).collect());
        let owner = self
            .owner
            .map(|(uid, gid)| identity(uid, gid))
            .unwrap_or(JValue::Null);
        let mut record = object(vec![
            ("taskId", string(id)),
            ("repo", string(repo_text)),
            ("branch", string(name)),
            ("path", string(fs::canonicalize(&tree)?.to_string_lossy())),
            ("home", string(home.to_string_lossy())),
            ("status", string("held")),
            ("mode", string(if clone { "clone" } else { "worktree" })),
            ("baseSha", base_sha.map(string).unwrap_or(JValue::Null)),
            ("owner", owner),
            (
                "noevia",
                identity(
                    rustix::process::getuid().as_raw(),
                    rustix::process::getgid().as_raw(),
                ),
            ),
            ("capabilities", paths(capabilities)),
            ("domains", paths(domains)),
            ("pinned", paths(&pins)),
            ("epoch", string(self.epoch.clone())),
            ("claimedAt", JValue::Num(now as f64)),
        ]);
        if let (Some(base_branch), JValue::Obj(fields)) = (base_branch, &mut record) {
            fields.push(("baseBranch".into(), string(base_branch)));
        }
        self.write(&record)?;
        undo.armed = false;
        Ok(record)
    }
}
fn identity(uid: u32, gid: u32) -> JValue {
    object(vec![
        ("uid", JValue::Num(f64::from(uid))),
        ("gid", JValue::Num(f64::from(gid))),
    ])
}
fn git_reason(error: Error) -> Error {
    let text = match &error {
        Error::Git(git::Error::Failed { stderr }) => stderr.as_str(),
        _ => return error,
    };
    let lower = text.to_ascii_lowercase();
    if lower.contains("dubious ownership") {
        return refused(
            400,
            "That repository is owned by another user and git refused to read it.",
        );
    }
    if lower.contains("not a git repository") {
        return refused(400, "Not a git repository");
    }
    let first = text
        .split('\n')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("unknown error");
    refused(
        400,
        format!(
            "git could not read that repository: {}",
            wtf16::slice(first, 200)
        ),
    )
}
fn chown(path: &Path, uid: u32, gid: u32) -> Result<(), std::io::Error> {
    let metadata = fs::symlink_metadata(path)?;
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
    if metadata.file_type().is_dir() {
        for entry in fs::read_dir(path)? {
            chown(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}
struct Undo<'a> {
    store: &'a Store,
    repo: &'a Path,
    tree: &'a Path,
    home: &'a Path,
    clone: bool,
    armed: bool,
}
impl Drop for Undo<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let cancelled = AtomicBool::new(false);
        if !self.clone {
            let _ = self.store.safe(
                self.repo,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    &self.tree.to_string_lossy(),
                ],
                &cancelled,
            );
            let _ = self
                .store
                .safe(self.repo, &["worktree", "prune"], &cancelled);
        }
        let _ = fs::remove_dir_all(self.tree);
        let _ = fs::remove_dir_all(self.home);
    }
}

fn set(record: &mut JValue, key: &str, value: JValue) {
    if let JValue::Obj(fields) = record {
        if let Some((_, old)) = fields.iter_mut().find(|(name, _)| name == key) {
            *old = value;
        } else {
            fields.push((key.into(), value));
        }
    }
}
fn field<'a>(record: &'a JValue, key: &str) -> Result<&'a str, Error> {
    record.get(key).as_str().ok_or(Error::Unavailable)
}
fn record_pins(record: &JValue) -> Vec<String> {
    match record.get("pinned") {
        JValue::Arr(values) => pinned_paths(
            &values
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect::<Vec<_>>(),
        ),
        _ => vec![],
    }
}
fn drop_pinned(record: &JValue) -> Result<(), (bool, String)> {
    let base = Path::new(
        record
            .get("path")
            .as_str()
            .ok_or((false, "Missing workspace path".into()))?,
    );
    for pin in record_pins(record) {
        let target = base.join(&pin);
        let mut dir = base.to_path_buf();
        let mut reachable = true;
        let components = Path::new(&pin).components().collect::<Vec<_>>();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            dir.push(component.as_os_str());
            let metadata = match fs::symlink_metadata(&dir) {
                Ok(m) => m,
                Err(_) => {
                    reachable = false;
                    break;
                }
            };
            if metadata.file_type().is_symlink() {
                return Err((
                    true,
                    format!(
                        "{} is a symbolic link",
                        dir.strip_prefix(base).unwrap_or(&dir).display()
                    ),
                ));
            }
            if !metadata.is_dir() {
                reachable = false;
                break;
            }
        }
        if !reachable {
            continue;
        }
        let metadata = match fs::symlink_metadata(&target) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.is_dir() {
            return Err((false, format!("{pin} is a directory")));
        }
        fs::remove_file(&target).map_err(|e| (false, e.to_string()))?;
    }
    Ok(())
}
fn dangerous_key(key: &str) -> bool {
    let key = key.trim().to_ascii_lowercase();
    [
        "filter.",
        "alias.",
        "include.",
        "includeif.",
        "uploadpack.",
        "receive.",
        "sendemail.",
    ]
    .iter()
    .any(|prefix| key.starts_with(prefix))
        || (key.starts_with("diff.") && (key.ends_with(".command") || key.ends_with(".textconv")))
        || (key.starts_with("merge.") && key.ends_with(".driver"))
        || (key.starts_with("credential.")
            && key.ends_with("helper")
            && (key == "credential.helper" || key.ends_with(".helper")))
        || [
            "core.fsmonitor",
            "core.hookspath",
            "core.sshcommand",
            "core.gitproxy",
            "core.worktree",
            "core.askpass",
            "core.editor",
            "core.pager",
        ]
        .contains(&key.as_str())
}
fn attr_files(dir: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    if depth > 64 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name() == ".git" {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            attr_files(&entry.path(), depth + 1, files);
        } else if entry.file_name() == ".gitattributes" {
            files.push(entry.path());
        }
    }
}
impl Store {
    fn hostile_reason(&self, tree: &Path, cancelled: &AtomicBool) -> Option<String> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tree.join(".git");
        let metadata = match fs::symlink_metadata(&dir) {
            Ok(m) => m,
            Err(_) => return Some("the clone has no .git directory".into()),
        };
        if !metadata.file_type().is_dir() {
            return Some("the clone’s .git is not a plain directory".into());
        }
        let config = dir.join("config");
        let listing = match self.run(
            tree,
            &[
                "config",
                "--file",
                &config.to_string_lossy(),
                "--name-only",
                "--list",
            ],
            cancelled,
        ) {
            Ok(s) => s,
            Err(e) => {
                return Some(format!(
                    "its .git/config could not be read: {}",
                    error_text(&e).split('\n').next().unwrap_or("unknown error")
                ))
            }
        };
        let mut bad = Vec::new();
        for key in listing
            .lines()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|k| dangerous_key(k))
        {
            if !bad.contains(&key) && bad.len() < 5 {
                bad.push(key);
            }
        }
        if !bad.is_empty() {
            return Some(format!(
                "its .git/config sets {}, which would make git run a command",
                bad.join(", ")
            ));
        }
        if let Ok(entries) = fs::read_dir(dir.join("hooks")) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.ends_with(".sample") {
                    continue;
                }
                if fs::metadata(entry.path())
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                {
                    return Some(format!(".git/hooks/{name} is an executable hook"));
                }
            }
        }
        let mut attributes = vec![dir.join("info/attributes")];
        attr_files(tree, 0, &mut attributes);
        for path in attributes {
            if let Ok(body) = fs::read_to_string(&path) {
                let mut start = 0;
                while let Some(at) = body.get(start..).and_then(|tail| tail.find("filter=")) {
                    let at = start + at;
                    if at == 0
                        || body
                            .get(..at)
                            .and_then(|s| s.chars().next_back())
                            .is_some_and(js_space)
                    {
                        return Some(format!(
                            "{} names a filter driver",
                            path.strip_prefix(tree).unwrap_or(&path).display()
                        ));
                    }
                    start = at + 7;
                }
            }
        }
        None
    }
    pub fn branch_head(&self, record: &JValue, cancelled: &AtomicBool) -> Option<String> {
        let branch = record.get("branch").as_str()?;
        if !valid_branch(branch) {
            return None;
        }
        self.safe(
            Path::new(record.get("repo").as_str()?),
            &["rev-parse", "--verify", branch],
            cancelled,
        )
        .ok()
        .filter(|s| !s.is_empty())
    }
    fn ended(
        &self,
        mut record: JValue,
        status: &str,
        now: i64,
        error: Option<String>,
        cancelled: &AtomicBool,
    ) -> Result<JValue, Error> {
        let head = self
            .branch_head(&record, cancelled)
            .map(string)
            .unwrap_or(JValue::Null);
        set(&mut record, "status", string(status));
        set(&mut record, "releasedAt", JValue::Num(now as f64));
        set(&mut record, "headSha", head);
        set(
            &mut record,
            "error",
            error.map(string).unwrap_or(JValue::Null),
        );
        self.write(&record)?;
        Ok(record)
    }
    pub fn recover(&self, now: i64) -> Result<Vec<JValue>, Error> {
        let _transaction = TRANSACTION.lock().map_err(|_| Error::Unavailable)?;
        let mut recovered = Vec::new();
        for mut record in self.list()? {
            if record.get("status").as_str() != Some("held")
                || record.get("epoch").as_str() == Some(self.epoch.as_str())
            {
                continue;
            }
            set(&mut record, "status", string("interrupted"));
            set(&mut record, "interruptedAt", JValue::Num(now as f64));
            if record.get("owner").truthy() {
                if let (Some(path), Some(uid), Some(gid)) = (
                    record.get("path").as_str(),
                    integer(record.get("noevia").get("uid")),
                    integer(record.get("noevia").get("gid")),
                ) {
                    let _ = chown(Path::new(path), uid, gid);
                }
            }
            match drop_pinned(&record) {
                Ok(()) => set(&mut record, "pinnedDropped", JValue::Bool(true)),
                Err((_, message)) => {
                    set(&mut record, "pinnedDropped", JValue::Bool(false));
                    set(&mut record, "pinnedDropError", string(message));
                }
            }
            self.write(&record)?;
            recovered.push(record);
        }
        Ok(recovered)
    }
}
fn integer(value: &JValue) -> Option<u32> {
    if let JValue::Num(n) = value {
        if n.is_finite() && *n >= 0.0 && *n <= f64::from(u32::MAX) && n.fract() == 0.0 {
            return Some(*n as u32);
        }
    }
    None
}
pub(crate) fn error_text(error: &Error) -> String {
    match error {
        Error::Refused { message, .. } | Error::Coded { message, .. } => message.clone(),
        Error::Io(e) => e.to_string(),
        Error::Git(git::Error::Failed { stderr }) => stderr.clone(),
        other => format!("{other:?}"),
    }
}

impl Store {
    pub fn release(
        &self,
        id: &str,
        remove_branch: bool,
        now: i64,
    ) -> Result<Option<JValue>, Error> {
        let _transaction = TRANSACTION.lock().map_err(|_| Error::Unavailable)?;
        let Some(record) = self.read(id)? else {
            return Ok(None);
        };
        if record.get("status").as_str() == Some("released") {
            return Ok(Some(record));
        }
        let cancelled = AtomicBool::new(false);
        let tree = PathBuf::from(field(&record, "path")?);
        let repo = PathBuf::from(field(&record, "repo")?);
        let branch = field(&record, "branch")?.to_owned();
        if !valid_branch(&branch) {
            return Err(Error::Unavailable);
        }
        let stuck = |message: String| {
            self.ended(record.clone(), "stuck", now, Some(message), &cancelled)
                .map(Some)
        };
        let clone = record.get("mode").as_str() == Some("clone");
        let mut removal_error = None;
        if clone {
            if record.get("owner").truthy() && record.get("noevia").truthy() {
                let uid = integer(record.get("noevia").get("uid")).ok_or(Error::Unavailable)?;
                let gid = integer(record.get("noevia").get("gid")).ok_or(Error::Unavailable)?;
                if let Err(e) = chown(&tree, uid, gid) {
                    return stuck(format!(
                        "Could not take the workspace back from the harness user: {e}"
                    ));
                }
            }
            if let Some(reason) = self.hostile_reason(&tree, &cancelled) {
                return stuck(format!("Refused to release the workspace: {reason}. It was left in place for inspection."));
            }
            if let Err((_, message)) = drop_pinned(&record) {
                return stuck(format!(
                    "Could not remove the harness configuration before saving: {message}"
                ));
            }
            let save = (|| {
                if !self
                    .safe(&tree, &["status", "--porcelain"], &cancelled)?
                    .is_empty()
                {
                    self.safe(&tree, &["add", "--all"], &cancelled)?;
                    self.safe(&tree,&["-c","user.name=noevia","-c","user.email=noevia@localhost","commit","--quiet","--no-verify","-m",&format!("noevia: work in progress from task {id}\n\nCommitted by noevia when the task ended, because the harness left it uncommitted.")],&cancelled)?;
                }
                Ok::<(), Error>(())
            })();
            if let Err(e) = save {
                return stuck(format!(
                    "Could not save the task’s uncommitted changes: {}",
                    error_text(&e)
                ));
            }
            if let Err(e) = self.safe(
                &repo,
                &[
                    "fetch",
                    "--quiet",
                    &tree.to_string_lossy(),
                    &format!("{branch}:{branch}"),
                ],
                &cancelled,
            ) {
                let message = error_text(&e);
                let lower = message.to_ascii_lowercase();
                if !lower.contains("couldn't find remote ref")
                    && !lower.contains("not found in upstream")
                {
                    return stuck(format!("Could not save the task’s branch: {message}"));
                }
            }
            if let Err(e) = fs::remove_dir_all(&tree) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    removal_error = Some(e.to_string());
                }
            }
            if let Some(home) = record.get("home").as_str() {
                let _ = fs::remove_dir_all(home);
            }
        } else {
            if let Err((link, message)) = drop_pinned(&record) {
                if link {
                    return stuck(format!("Refused to release the workspace: {message}. It was left in place for inspection."));
                }
            }
            if let Err(e) = self.safe(
                &repo,
                &["worktree", "remove", "--force", &tree.to_string_lossy()],
                &cancelled,
            ) {
                removal_error = Some(error_text(&e));
            }
            let _ = self.safe(&repo, &["worktree", "prune"], &cancelled);
        }
        if removal_error.is_none() && remove_branch {
            let _ = self.run(&repo, &["branch", "-D", &branch], &cancelled);
        }
        self.ended(
            record,
            if removal_error.is_none() {
                "released"
            } else {
                "stuck"
            },
            now,
            removal_error,
            &cancelled,
        )
        .map(Some)
    }
}

fn js_space(c: char) -> bool {
    let mut bytes = [0; 4];
    server_account::util::js_trim(c.encode_utf8(&mut bytes)).is_empty()
}

impl Store {
    fn prepare_tree(
        &self,
        tree: &Path,
        home: &Path,
        pinned: &[String],
        cancelled: &AtomicBool,
    ) -> Result<Vec<String>, Error> {
        let pins = pinned_paths(pinned);
        if !pins.is_empty() {
            let mut args = vec!["ls-files", "-z", "--"];
            args.extend(pins.iter().map(String::as_str));
            let tracked = self.safe(tree, &args, cancelled).map_err(git_reason)?;
            let names = tracked
                .split('\0')
                .filter(|s| !s.is_empty())
                .take(3)
                .collect::<Vec<_>>();
            if !names.is_empty() {
                return Err(refused(409,format!("This repository tracks {}, which noevia writes the harness configuration (and its key) into. Remove it from the repository to run this harness.",names.join(", "))));
            }
        }
        let exclude = self.safe(
            tree,
            &["rev-parse", "--git-path", "info/exclude"],
            cancelled,
        )?;
        if exclude.is_empty() {
            return Err(refused(500,"Could not keep the harness configuration out of git: git did not say where its exclude file is"));
        }
        let exclude = tree.join(exclude);
        if let Some(parent) = exclude.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = open(&exclude, true)?;
        let mut current = String::new();
        file.read_to_string(&mut current)?;
        let mut lines = LEAVINGS.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        lines.extend(pins.iter().map(|p| format!("/{p}")));
        let missing = lines
            .iter()
            .filter(|s| !current.split('\n').any(|line| line == s.as_str()))
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            file.write_all(b"\n# noevia: harness state, not the task's work\n")?;
            for line in missing {
                file.write_all(line.as_bytes())?;
                file.write_all(b"\n")?;
            }
        }
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::Git(git::Error::Cancelled));
        }
        if let Some((uid, gid)) = self.owner {
            chown(tree, uid, gid)
                .and_then(|()| chown(home, uid, gid))
                .map_err(|e| {
                    refused(
                        500,
                        format!("Could not hand the workspace to the harness user: {e}"),
                    )
                })?;
        }
        Ok(pins)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    const USER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const TASK: &str = "11111111-1111-4111-8111-111111111111";
    const NEXT: &str = "22222222-2222-4222-8222-222222222222";
    fn init(dir: &Path) -> (PathBuf, CodeConfig) {
        let repo = dir.join("repo");
        fs::create_dir(&repo).unwrap();
        let env = BTreeMap::from([
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
            ("HOME".into(), dir.to_string_lossy().into_owned()),
        ]);
        let cancelled = AtomicBool::new(false);
        let run = |args: &[&str]| {
            git::run(
                &args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
                &repo,
                &env,
                1024 * 1024,
                false,
                &cancelled,
            )
            .unwrap()
        };
        run(&["init", "--quiet", "--initial-branch=main"]);
        fs::write(repo.join("file.txt"), "original").unwrap();
        run(&["add", "--all"]);
        run(&[
            "-c",
            "user.name=synthetic",
            "-c",
            "user.email=synthetic@invalid",
            "commit",
            "--quiet",
            "-m",
            "base",
        ]);
        fs::create_dir_all(dir.join("users").join(USER)).unwrap();
        let mut config = CodeConfig::from_env(|_| None);
        config.owner = Some((
            rustix::process::getuid().as_raw(),
            rustix::process::getgid().as_raw(),
        ));
        config.tree_root = Some(dir.join("trees"));
        (repo, config)
    }
    #[test]
    fn native_workspace_matches_node_clone_save_and_restart_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let (repo, config) = init(dir.path());
        let expected = wtf16::parse(include_str!("../tests/fixtures/workspace-node.json")).unwrap();
        let store = Store::open(dir.path(), USER, &config, "first".into()).unwrap();
        let cancelled = AtomicBool::new(false);
        let claim = store
            .claim(
                TASK,
                &repo,
                None,
                &["read_repository".into(), "edit_file".into()],
                &["example.com".into()],
                &["engine-config.json".into()],
                1234,
                &cancelled,
            )
            .unwrap();
        for key in [
            "taskId",
            "branch",
            "status",
            "mode",
            "baseBranch",
            "capabilities",
            "domains",
            "pinned",
            "epoch",
            "claimedAt",
        ] {
            assert_eq!(claim.get(key), expected.get("created").get(key), "{key}");
        }
        assert_eq!(
            JValue::Bool(full_sha(field(&claim, "baseSha").unwrap())),
            *expected.get("created").get("hasBase")
        );
        let tree = PathBuf::from(field(&claim, "path").unwrap());
        let home = PathBuf::from(field(&claim, "home").unwrap());
        assert_eq!(
            store.project_snapshot(TASK, 400, 8192, &cancelled).unwrap(),
            *expected.get("pipeline").get("snapshot")
        );
        fs::write(tree.join("file.txt"), "changed").unwrap();
        fs::write(
            tree.join("engine-config.json"),
            "synthetic credential placeholder",
        )
        .unwrap();
        let released = store.release(TASK, false, 1234).unwrap().unwrap();
        for key in ["status", "error", "releasedAt"] {
            assert_eq!(released.get(key), expected.get("saved").get(key), "{key}");
        }
        assert_eq!(
            JValue::Bool(released.get("headSha") != released.get("baseSha")),
            *expected.get("saved").get("changedHead")
        );
        let saved = store
            .safe(
                &repo,
                &[
                    "show",
                    &format!("{}:file.txt", field(&claim, "branch").unwrap()),
                ],
                &cancelled,
            )
            .unwrap();
        assert_eq!(JValue::Str(saved), *expected.get("saved").get("saved"));
        assert_eq!(
            JValue::Str(fs::read_to_string(repo.join("file.txt")).unwrap()),
            *expected.get("saved").get("original")
        );
        assert_eq!(
            JValue::Bool(!tree.exists()),
            *expected.get("saved").get("treeRemoved")
        );
        assert_eq!(
            JValue::Bool(!home.exists()),
            *expected.get("saved").get("homeRemoved")
        );
        // Release is idempotent and must never inspect the removed tree again.
        assert_eq!(store.release(TASK, false, 5678).unwrap().unwrap(), released);
        let normalize = |value: &JValue| {
            let root = fs::canonicalize(dir.path()).unwrap();
            let text = wtf16::stringify(value)
                .unwrap()
                .replace(root.to_str().unwrap(), "<ROOT>")
                .replace(field(&released, "baseSha").unwrap(), "<base>")
                .replace(field(&released, "headSha").unwrap(), "<head>");
            wtf16::parse(&text).unwrap()
        };
        assert_eq!(
            normalize(&store.change(TASK, 200, &cancelled).unwrap()),
            *expected.get("pipeline").get("change")
        );
        assert_eq!(
            normalize(
                &store
                    .verify_checkout(TASK, field(&released, "headSha").unwrap(), &cancelled)
                    .unwrap()
            ),
            *expected.get("pipeline").get("verified")
        );
        assert_eq!(
            normalize(&store.merge_preflight(TASK, field(&released, "headSha").unwrap())),
            *expected.get("pipeline").get("checkedOut")
        );
        store
            .safe(&repo, &["checkout", "--detach", "--quiet"], &cancelled)
            .unwrap();
        assert_eq!(
            store.merge_preflight(TASK, field(&released, "headSha").unwrap()),
            *expected.get("pipeline").get("preflight")
        );
        assert_eq!(
            normalize(&store.merge_verified(TASK, field(&released, "headSha").unwrap(), 1234)),
            *expected.get("pipeline").get("merged")
        );
        let resumed = store
            .reclaim(TASK, &["engine-config.json".into()], 1234, &cancelled)
            .unwrap();
        assert_eq!(resumed.get("status"), expected.get("reclaim").get("status"));
        assert_eq!(resumed.get("rounds"), expected.get("reclaim").get("rounds"));
        assert_eq!(
            JValue::Bool(resumed.get("baseSha") == released.get("baseSha")),
            *expected.get("reclaim").get("baseKept")
        );
        assert_eq!(
            JValue::Bool(resumed.get("branch") == claim.get("branch")),
            *expected.get("reclaim").get("branchKept")
        );
        assert_eq!(
            JValue::Str(
                fs::read_to_string(Path::new(field(&resumed, "path").unwrap()).join("file.txt"))
                    .unwrap()
            ),
            *expected.get("reclaim").get("content")
        );
        store.release(TASK, false, 1234).unwrap();
        let next = store
            .claim(
                NEXT,
                &repo,
                None,
                &[],
                &[],
                &["engine-config.json".into()],
                1234,
                &cancelled,
            )
            .unwrap();
        let next_tree = PathBuf::from(field(&next, "path").unwrap());
        fs::write(
            next_tree.join("engine-config.json"),
            "synthetic credential placeholder",
        )
        .unwrap();
        assert!(matches!(
            Store::open(dir.path(), USER, &config, "overlap".into()),
            Err(Error::Busy)
        ));
        drop(store);
        let restarted = Store::open(dir.path(), USER, &config, "second".into()).unwrap();
        let recovered = restarted.recover(5678).unwrap();
        let recovered = recovered.first().unwrap();
        assert!(matches!(
            restarted.reclaim(NEXT, &[], 5678, &cancelled),
            Err(Error::Refused { status: 409, .. })
        ));
        for key in ["status", "interruptedAt", "pinnedDropped"] {
            assert_eq!(
                recovered.get(key),
                expected.get("recovery").get(key),
                "{key}"
            );
        }
        assert_eq!(
            JValue::Bool(next_tree.exists()),
            *expected.get("recovery").get("treeKept")
        );
        assert_eq!(
            JValue::Bool(!next_tree.join("engine-config.json").exists()),
            *expected.get("recovery").get("pinRemoved")
        );
    }
    #[test]
    fn hostile_keys_and_pin_paths_match_existing_policy() {
        for key in [
            "filter.x.clean",
            "core.fsmonitor",
            "credential.helper",
            "credential.https://example.com.helper",
            "diff.x.textconv",
            "includeif.gitdir:x.path",
            "alias.x",
        ] {
            assert!(dangerous_key(key), "{key}");
        }
        for key in [
            "core.filemode",
            "remote.origin.url",
            "branch.main.merge",
            "credential.username",
        ] {
            assert!(!dangerous_key(key), "{key}");
        }
        assert_eq!(
            pinned_paths(&[
                "a/b.json".into(),
                "../outside".into(),
                "/absolute".into(),
                "a/../../b".into(),
                "a\\..\\b".into()
            ]),
            vec!["a/b.json"]
        );
    }
}
