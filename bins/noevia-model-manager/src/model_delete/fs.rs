//! The file side of `services.delete_gguf`: what is removed, what is protected, and how much was
//! freed. Paths are named as Python names them (`settings.models_dir / ...`, for messages and the
//! protected-folder rules); every read and removal goes through directory handles opened from the
//! canonical models folder one component at a time without following links.
use super::{normpath, own_rels, py_strip, Config, Deleted};
use crate::model_detail::Entry;
use crate::model_updates::companion;
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use std::{
    collections::HashSet,
    os::fd::OwnedFd,
    path::{Path, PathBuf},
};

/// Most entries one deletion may look at (each walk counts against it).
const WALK_CAP: usize = 1_000_000;
/// `MAX_DEPTH` for the walks below a folder (the snapshot itself reads four levels).
const DEPTH_CAP: usize = 64;

#[derive(Debug)]
pub(super) struct Failure(std::io::Error, String);

type Res<T> = Result<T, Failure>;

fn io(e: rustix::io::Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(e.raw_os_error())
}

/// `str(OSError)`: `[Errno n] strerror: 'path'`.
fn py_os_error(e: &std::io::Error, path: &str) -> String {
    let Some(errno) = e.raw_os_error() else {
        return e.to_string();
    };
    let text = e.to_string();
    let strerror = text
        .rsplit_once(" (os error ")
        .map_or(text.as_str(), |(head, _)| head);
    let repr = crate::prompts::py_repr_str(path).unwrap_or_else(|_| format!("'{path}'"));
    format!("[Errno {errno}] {strerror}: {repr}")
}

impl Failure {
    fn text(&self) -> String {
        py_os_error(&self.0, &self.1)
    }
}

/// What one directory entry is, from `lstat` and (for a link) `stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Dir,
    File(u64),
    /// A symbolic link: whether what it points at is a file or a folder.
    Link {
        file: bool,
        dir: bool,
    },
    Other,
}

impl Kind {
    /// `Path.is_file()`.
    fn is_file(self) -> bool {
        matches!(self, Kind::File(_) | Kind::Link { file: true, .. })
    }
    /// `Path.is_dir()` (follows a link).
    fn is_dir(self) -> bool {
        matches!(self, Kind::Dir | Kind::Link { dir: true, .. })
    }
}

/// The models folder and the walk budget of one deletion.
struct Tree<'a> {
    config: &'a Config,
    base: OwnedFd,
    /// The canonical models folder.
    real: PathBuf,
    budget: usize,
    mounts: Option<Option<HashSet<String>>>,
}

impl Tree<'_> {
    /// `settings.models_dir / rel` as Python spells it.
    fn path(&self, rel: &str) -> String {
        if rel.is_empty() {
            self.config.models.clone()
        } else {
            format!("{}/{rel}", self.config.models)
        }
    }

    fn spend(&mut self, rel: &str) -> Res<()> {
        if self.budget == 0 {
            return Err(Failure(
                std::io::Error::other("too many files to check"),
                self.path(rel),
            ));
        }
        self.budget -= 1;
        Ok(())
    }

    /// The folder `rel` (`""`: the models folder), never through a link.
    fn dir(&self, rel: &str) -> Res<OwnedFd> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut fd = rustix::fs::openat(&self.base, ".", flags, Mode::empty())
            .map_err(|e| Failure(io(e), self.path(rel)))?;
        for part in rel.split('/').filter(|p| !p.is_empty()) {
            if part == "." || part == ".." {
                return Err(Failure(
                    std::io::Error::other("unexpected path segment"),
                    self.path(rel),
                ));
            }
            fd = rustix::fs::openat(&fd, part, flags, Mode::empty())
                .map_err(|e| Failure(io(e), self.path(rel)))?;
        }
        Ok(fd)
    }

    /// The folder holding `rel` and its last name.
    fn parent(&self, rel: &str) -> Res<(OwnedFd, String)> {
        let (folder, name) = rel.rsplit_once('/').unwrap_or(("", rel));
        Ok((self.dir(folder)?, name.to_owned()))
    }

    fn kind_at(dir: &OwnedFd, name: &str) -> std::io::Result<Kind> {
        let st = rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io)?;
        Ok(match FileType::from_raw_mode(st.st_mode) {
            FileType::Directory => Kind::Dir,
            #[allow(clippy::cast_sign_loss)]
            FileType::RegularFile => Kind::File(st.st_size as u64),
            FileType::Symlink => match rustix::fs::statat(dir, name, AtFlags::empty()) {
                Ok(target) => {
                    let kind = FileType::from_raw_mode(target.st_mode);
                    Kind::Link {
                        file: kind == FileType::RegularFile,
                        dir: kind == FileType::Directory,
                    }
                }
                Err(_) => Kind::Link {
                    file: false,
                    dir: false,
                },
            },
            _ => Kind::Other,
        })
    }

    /// What `rel` is; `None` when it does not exist.
    fn kind(&self, rel: &str) -> Res<Option<Kind>> {
        let (dir, name) = match self.parent(rel) {
            Ok(found) => found,
            Err(Failure(e, _)) if missing(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        match Self::kind_at(&dir, &name) {
            Ok(kind) => Ok(Some(kind)),
            Err(e) if missing(&e) => Ok(None),
            Err(e) => Err(Failure(e, self.path(rel))),
        }
    }

    /// The names in a folder with what each is, sorted by name.
    fn list(&mut self, rel: &str) -> Res<Vec<(String, Kind)>> {
        let fd = self.dir(rel)?;
        let mut out = Vec::new();
        let dir = rustix::fs::Dir::read_from(&fd).map_err(|e| Failure(io(e), self.path(rel)))?;
        for item in dir {
            let item = item.map_err(|e| Failure(io(e), self.path(rel)))?;
            let name = item.file_name().to_string_lossy().into_owned();
            if name == "." || name == ".." {
                continue;
            }
            self.spend(rel)?;
            let kind =
                Self::kind_at(&fd, &name).map_err(|e| Failure(e, self.path(&join(rel, &name))))?;
            out.push((name, kind));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// `Path.rglob("*")` below `rel`: every entry, never descending a linked folder.
    fn rglob(&mut self, rel: &str, depth: usize, out: &mut Vec<(String, Kind)>) -> Res<()> {
        if depth > DEPTH_CAP {
            return Err(Failure(
                std::io::Error::other("folders nested too deep"),
                self.path(rel),
            ));
        }
        for (name, kind) in self.list(rel)? {
            let child = join(rel, &name);
            out.push((child.clone(), kind));
            if kind == Kind::Dir {
                self.rglob(&child, depth + 1, out)?;
            }
        }
        Ok(())
    }

    /// `/proc/self/mountinfo` field 5 of each line (`None`: unreadable), read once.
    fn mount_points(&mut self) -> Option<&HashSet<String>> {
        if self.mounts.is_none() {
            self.mounts = Some(read_mountinfo(&self.config.mountinfo));
        }
        self.mounts.as_ref().and_then(Option::as_ref)
    }

    /// `_is_mount_point(path)`.
    fn is_mount_point(&mut self, rel: &str) -> bool {
        let path = self.path(rel);
        if ismount(&path) {
            return true;
        }
        let Some(points) = self.mount_points() else {
            return false;
        };
        if points.is_empty() {
            return false;
        }
        if points.contains(&normpath(&path)) {
            return true;
        }
        std::fs::canonicalize(&path)
            .ok()
            .and_then(|p| p.to_str().map(str::to_owned))
            .is_some_and(|real| points.contains(&real))
    }

    /// `_protected_dir(path)`.
    fn protected(&mut self, rel: &str) -> bool {
        let root = normpath(&self.config.models);
        let here = normpath(&self.path(rel));
        if here == root || !here.starts_with(&format!("{}/", root.trim_end_matches('/'))) {
            return true;
        }
        for name in self.config.targets.split(',') {
            let name = py_strip(name).trim_matches('/');
            if !name.is_empty() && normpath(&format!("{root}/{name}")) == here {
                return true;
            }
        }
        match self.kind(rel) {
            Ok(Some(Kind::Link { .. })) => true,
            Ok(_) => self.is_mount_point(rel),
            Err(_) => true,
        }
    }

    /// `_other_models_below(folder, own)`: any GGUF below `rel` that is not this model's.
    fn other_models_below(&mut self, rel: &str, own: &HashSet<String>) -> bool {
        self.below(rel, rel, own, 0).unwrap_or(true)
    }

    fn below(&mut self, top: &str, rel: &str, own: &HashSet<String>, depth: usize) -> Res<bool> {
        let nested = rel != top;
        if nested && self.is_mount_point(rel) {
            return Ok(true); // a filesystem mounted inside: rmtree would empty it
        }
        if depth > DEPTH_CAP {
            return Ok(true);
        }
        let entries = self.list(rel)?;
        // os.walk: what `is_dir()` (following links) calls a folder is not a file name.
        for (name, kind) in &entries {
            if kind.is_dir() || !name.to_lowercase().ends_with(".gguf") {
                continue;
            }
            if own.contains(&join(rel, name)) {
                continue;
            }
            if nested || !companion(name) {
                return Ok(true);
            }
        }
        for (name, kind) in &entries {
            if *kind == Kind::Dir && self.below(top, &join(rel, name), own, depth + 1)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `_other_models_beside(match)`: another main model directly in this model's folder.
    fn other_models_beside(&mut self, entry: &Entry) -> bool {
        let folder = entry.model.subdir.clone();
        let own: HashSet<String> = entry
            .model
            .parts
            .iter()
            .map(|p| join(&folder, &p.name))
            .collect();
        match self.list(&folder) {
            Ok(entries) => entries.iter().any(|(name, kind)| {
                kind.is_file()
                    && name.to_lowercase().ends_with(".gguf")
                    && !own.contains(&join(&folder, name))
                    && !companion(name)
            }),
            Err(_) => true,
        }
    }

    fn unlink(&self, rel: &str) -> Res<()> {
        let (dir, name) = self.parent(rel)?;
        rustix::fs::unlinkat(&dir, name.as_str(), AtFlags::empty())
            .map_err(|e| Failure(io(e), self.path(rel)))
    }

    fn rmdir(&self, rel: &str) -> Res<()> {
        let (dir, name) = self.parent(rel)?;
        rustix::fs::unlinkat(&dir, name.as_str(), AtFlags::REMOVEDIR)
            .map_err(|e| Failure(io(e), self.path(rel)))
    }

    fn empty(&mut self, rel: &str) -> Res<bool> {
        Ok(self.list(rel)?.is_empty())
    }

    /// `shutil.rmtree(path)`: links are removed, never followed.
    fn rmtree(&mut self, rel: &str, depth: usize) -> Res<()> {
        if depth > DEPTH_CAP {
            return Err(Failure(
                std::io::Error::other("folders nested too deep"),
                self.path(rel),
            ));
        }
        for (name, kind) in self.list(rel)? {
            let child = join(rel, &name);
            if kind == Kind::Dir {
                self.rmtree(&child, depth + 1)?;
            } else {
                self.unlink(&child)?;
            }
        }
        self.rmdir(rel)
    }

    /// Sizes and names `delete_gguf` counts before a folder goes: files that are not links add
    /// their size, files and links add their name.
    fn tally(&mut self, rel: &str) -> Res<(u128, usize)> {
        let mut all = Vec::new();
        self.rglob(rel, 0, &mut all)?;
        let mut freed = 0u128;
        let mut removed = 0;
        for (_, kind) in all {
            if let Kind::File(size) = kind {
                freed += u128::from(size);
            }
            if kind.is_file() || matches!(kind, Kind::Link { .. }) {
                removed += 1;
            }
        }
        Ok((freed, removed))
    }

    /// `Path.resolve()` of a link inside the models folder (`None`: it cannot be resolved).
    fn resolve(&self, rel: &str) -> Option<PathBuf> {
        std::fs::canonicalize(self.path(rel)).ok()
    }
}

fn missing(e: &std::io::Error) -> bool {
    e.raw_os_error().is_some_and(|code| {
        code == rustix::io::Errno::NOENT.raw_os_error()
            || code == rustix::io::Errno::NOTDIR.raw_os_error()
    })
}

fn join(rel: &str, name: &str) -> String {
    if rel.is_empty() {
        name.to_owned()
    } else {
        format!("{rel}/{name}")
    }
}

/// `os.path.ismount(path)`.
fn ismount(path: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(here) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if here.file_type().is_symlink() {
        return false;
    }
    let Ok(parent) = std::fs::symlink_metadata(format!("{path}/..")) else {
        return false;
    };
    here.dev() != parent.dev() || here.ino() == parent.ino()
}

/// `_mount_points()`: field 5 of each mountinfo line, `\NNN` octal escapes decoded.
fn read_mountinfo(path: &Path) -> Option<HashSet<String>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(16 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut points = HashSet::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        if let Some(field) = fields.get(4) {
            points.insert(normpath(&unescape(field)));
        }
    }
    Some(points)
}

fn unescape(field: &str) -> String {
    let chars: Vec<char> = field.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let digits = chars.get(i + 1..i + 4);
        if c == '\\' {
            if let Some(d) = digits.filter(|d| d.iter().all(|c| ('0'..='7').contains(c))) {
                let value = d
                    .iter()
                    .fold(0u32, |n, c| n * 8 + c.to_digit(8).unwrap_or(0));
                if let Some(decoded) = char::from_u32(value) {
                    out.push(decoded);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `[n for n in names]` as Python prints a list of str.
fn py_list(names: &[String]) -> String {
    let items: Vec<String> = names
        .iter()
        .map(|n| crate::prompts::py_repr_str(n).unwrap_or_else(|_| format!("'{n}'")))
        .collect();
    format!("[{}]", items.join(", "))
}

/// `name` of a models-relative path.
fn name_of(rel: &str) -> &str {
    rel.rsplit_once('/').map_or(rel, |(_, n)| n)
}

/// `_HF_CACHE_RE.match(subdir)`: the repository folder of a Hugging Face cache snapshot.
pub(super) fn hf_repo(subdir: &str) -> Option<String> {
    let parts: Vec<&str> = subdir.split('/').collect();
    for i in 0..parts.len() {
        let (Some(repo), Some(snaps), Some(_rev)) =
            (parts.get(i), parts.get(i + 1), parts.get(i + 2))
        else {
            break;
        };
        if repo.len() > 8 && repo.starts_with("models--") && *snaps == "snapshots" {
            return Some(parts.get(..=i)?.join("/"));
        }
    }
    None
}

/// `delete_gguf` from the point where the model and its holders are settled.
pub(super) fn delete(config: &Config, base: &Path, entry: &Entry) -> Deleted {
    let failed = |message: String| (false, message, 0);
    let fd = match rustix::fs::open(
        base,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) => return failed(py_os_error(&io(e), &config.models)),
    };
    let mut tree = Tree {
        config,
        base: fd,
        real: base.to_owned(),
        budget: WALK_CAP,
        mounts: None,
    };
    let subdir = entry.model.subdir.as_str();
    if let Some(repo) = hf_repo(subdir) {
        return hf_snapshot(&mut tree, entry, &repo);
    }
    let own: HashSet<String> = own_rels(entry).into_iter().collect();
    if !subdir.is_empty()
        && matches!(tree.kind(subdir), Ok(Some(k)) if k.is_dir())
        && !tree.protected(subdir)
        && !tree.other_models_below(subdir, &own)
    {
        let (freed, removed) = match tree.tally(subdir) {
            Ok(t) => t,
            Err(e) => return failed(format!("failed to remove {subdir}/: {}", e.text())),
        };
        return match tree.rmtree(subdir, 0) {
            Ok(()) => (
                true,
                format!("deleted {removed} file(s), removed {subdir}/"),
                freed,
            ),
            Err(e) => (
                false,
                format!("failed to remove {subdir}/: {}", e.text()),
                freed,
            ),
        };
    }
    let mut victims: Vec<String> = entry
        .model
        .parts
        .iter()
        .map(|p| join(subdir, &p.name))
        .collect();
    if !entry.model.companion_parts.is_empty() && !tree.other_models_beside(entry) {
        victims.extend(
            entry
                .model
                .companion_parts
                .iter()
                .map(|p| join(subdir, &p.name)),
        );
    }
    let mut freed = 0u128;
    let mut removed: Vec<String> = Vec::new();
    for rel in victims {
        let step = (|| -> Res<u128> {
            let size = match tree.kind(&rel)? {
                Some(Kind::Link { .. }) => 0,
                Some(Kind::File(size)) => u128::from(size),
                Some(_) => 0,
                None => {
                    return Err(Failure(
                        std::io::Error::from_raw_os_error(rustix::io::Errno::NOENT.raw_os_error()),
                        tree.path(&rel),
                    ))
                }
            };
            tree.unlink(&rel)?;
            Ok(size)
        })();
        match step {
            Ok(size) => {
                freed += size;
                removed.push(name_of(&rel).to_owned());
            }
            Err(e) => {
                return (
                    false,
                    format!(
                        "failed to delete {}: {}. removed so far: {}",
                        name_of(&rel),
                        e.text(),
                        py_list(&removed)
                    ),
                    freed,
                )
            }
        }
    }
    if !subdir.is_empty()
        && matches!(tree.kind(subdir), Ok(Some(k)) if k.is_dir())
        && !tree.protected(subdir)
        && matches!(tree.empty(subdir), Ok(true))
    {
        let _ = tree.rmdir(subdir);
    }
    (true, format!("deleted {} file(s)", removed.len()), freed)
}

/// `_delete_hf_snapshot(match, repo_dir)`.
fn hf_snapshot(tree: &mut Tree<'_>, entry: &Entry, repo: &str) -> Deleted {
    let folder = entry.model.subdir.clone();
    let snaps = format!("{repo}/snapshots");
    let own: HashSet<String> = own_rels(entry).into_iter().collect();
    let failed_while = |freed: u128, e: &Failure| {
        (
            false,
            format!("failed while deleting {}: {}", entry.model.base, e.text()),
            freed,
        )
    };
    let victims: Vec<String> = if tree.other_models_below(&folder, &own) {
        let mut v: Vec<String> = entry
            .model
            .parts
            .iter()
            .map(|p| join(&folder, &p.name))
            .collect();
        if !tree.other_models_beside(entry) {
            v.extend(
                entry
                    .model
                    .companion_parts
                    .iter()
                    .map(|p| join(&folder, &p.name)),
            );
        }
        v
    } else {
        let mut all = Vec::new();
        if let Err(e) = tree.rglob(&folder, 0, &mut all) {
            return failed_while(0, &e);
        }
        let mut v: Vec<String> = all
            .into_iter()
            .filter(|(_, k)| k.is_file() || matches!(k, Kind::Link { .. }))
            .map(|(rel, _)| rel)
            .collect();
        v.sort();
        v
    };
    let victim_set: HashSet<&String> = victims.iter().collect();
    // Blobs still referenced by a link that is not being deleted.
    let mut in_use: HashSet<PathBuf> = HashSet::new();
    if matches!(tree.kind(&snaps), Ok(Some(k)) if k.is_dir()) {
        let mut all = Vec::new();
        if let Err(e) = tree.rglob(&snaps, 0, &mut all) {
            return failed_while(0, &e);
        }
        for (rel, kind) in all {
            if matches!(kind, Kind::Link { .. }) && !victim_set.contains(&rel) {
                if let Some(target) = tree.resolve(&rel) {
                    in_use.insert(target);
                }
            }
        }
    }
    let mut freed = 0u128;
    let mut removed = 0usize;
    let repo_real = tree.resolve(repo);
    for rel in &victims {
        let kind = match tree.kind(rel) {
            Ok(kind) => kind,
            Err(e) => return failed_while(freed, &e),
        };
        match kind {
            Some(Kind::Link { .. }) => {
                if let (Some(target), Some(repo_real)) = (tree.resolve(rel), repo_real.as_ref()) {
                    let inside_repo = target != *repo_real && target.starts_with(repo_real);
                    if inside_repo && !in_use.contains(&target) {
                        // Only a blob inside the canonical models folder is removed.
                        if let Some(blob) = target
                            .strip_prefix(&tree.real)
                            .ok()
                            .and_then(Path::to_str)
                            .map(str::to_owned)
                        {
                            if let Ok(Some(Kind::File(size))) = tree.kind(&blob) {
                                if let Err(e) = tree.unlink(&blob) {
                                    return failed_while(freed, &e);
                                }
                                freed += u128::from(size);
                            }
                        }
                    }
                }
            }
            Some(Kind::File(size)) => freed += u128::from(size),
            _ => {}
        }
        if let Err(e) = tree.unlink(rel) {
            return failed_while(freed, &e);
        }
        removed += 1;
    }
    // Tidy empty folders inside the model's folder, then from it up to (not including)
    // snapshots/.
    if matches!(tree.kind(&folder), Ok(Some(k)) if k.is_dir()) {
        let mut all = Vec::new();
        if let Err(e) = tree.rglob(&folder, 0, &mut all) {
            return failed_while(freed, &e);
        }
        let mut dirs: Vec<String> = all
            .into_iter()
            .filter(|(_, k)| *k == Kind::Dir)
            .map(|(rel, _)| rel)
            .collect();
        dirs.sort();
        dirs.sort_by_key(|d| std::cmp::Reverse(d.matches('/').count()));
        for d in dirs {
            match tree.empty(&d) {
                Ok(true) if !tree.protected(&d) => {
                    if let Err(e) = tree.rmdir(&d) {
                        return failed_while(freed, &e);
                    }
                }
                Ok(_) => {}
                Err(e) => return failed_while(freed, &e),
            }
        }
    }
    let mut here = folder.clone();
    while here != snaps && here.starts_with(&format!("{snaps}/")) && !tree.protected(&here) {
        if !matches!(tree.kind(&here), Ok(Some(k)) if k.is_dir()) {
            break;
        }
        match tree.empty(&here) {
            Ok(true) => {
                if let Err(e) = tree.rmdir(&here) {
                    return failed_while(freed, &e);
                }
                here = here
                    .rsplit_once('/')
                    .map_or(String::new(), |(p, _)| p.to_owned());
            }
            Ok(false) => break,
            Err(e) => return failed_while(freed, &e),
        }
    }
    let snaps_left = match tree.kind(&snaps) {
        Ok(Some(k)) if k.is_dir() => match tree.empty(&snaps) {
            Ok(empty) => !empty,
            Err(e) => return failed_while(freed, &e),
        },
        _ => false,
    };
    if !snaps_left && !tree.protected(repo) {
        let (more, _) = match tree.tally(repo) {
            Ok(t) => t,
            Err(e) => return failed_while(freed, &e),
        };
        freed += more;
        if let Err(e) = tree.rmtree(repo, 0) {
            return failed_while(freed, &e);
        }
        return (
            true,
            format!("deleted {removed} file(s), removed {repo}/"),
            freed,
        );
    }
    (true, format!("deleted {removed} file(s)"), freed)
}
