//! Writing the pinned files (`writeHarnessConfig`, `writePinned`).
//!
//! The repository is not noevia's: a checkout can carry a symlink named `.claude` or
//! `opencode.json`, and noevia runs as root, so a write through such a link would put the engine
//! key wherever the repository pointed and hand it to the harness user. Nothing below the root
//! may be a link, and the target is replaced rather than written in place.
//!
//! Since noevia-core#367 Node itself walks by directory descriptor (`dir-fd.cjs`, over
//! `/proc/self/fd/<n>`, which stands in for `openat`), and so does this port, natively with
//! `openat`/`fstatat`/`mkdirat`/`unlinkat`: the root is opened `O_DIRECTORY|O_NOFOLLOW` (a root
//! that is itself a link is refused by the open, like any other open error), each component is
//! classified with `fstatat(AT_SYMLINK_NOFOLLOW)` relative to the descriptor of its parent (a link
//! or a non-directory is refused with Node's messages), made with `mkdirat` mode `0o700` when
//! missing and entered with an `O_RDONLY|O_DIRECTORY|O_NOFOLLOW` open, which must give the
//! directory that was looked at (same device and inode) or the write is refused with "it was
//! replaced while it was being checked". The old file is removed with `unlinkat`, the new one
//! created `O_CREAT|O_EXCL|O_TRUNC|O_NOFOLLOW`, mode `0o600`, and handed over with `fchown` on its
//! own descriptor; then the directories this call made are handed over, innermost first, each
//! through its own descriptor, so nothing is handed to the harness user before the file is in
//! place and nothing can be planted in a directory that is not yet theirs. A failed hand-over
//! stops the rest (the file and the directories stay where they are), as in Node.
//!
//! Named difference from Node, `no-proc`: Node refuses a host without `/proc/self/fd` ("this host
//! has no /proc/self/fd, ..."), because it can only spell `openat` that way. This port does not
//! need `/proc` and works anywhere `openat` does, so that refusal has no counterpart (and no
//! fixture: the recorder runs on Linux). Named difference `typed-errors`: a Node `fs` error is a
//! [`PinError::Io`] with Node's syscall name (`open`, `lstat`, `mkdir`, `unlink`, `write`,
//! `fchown`), the OS error and, where Node has one, the path; a path component Node's `at()`
//! cannot spell (empty, `.`, `..` or containing a NUL) is [`PinError::NotAComponent`], Node's
//! plain `Error('not a single path component')`.
//!
//! The root is opened first, so a root that cannot be opened (missing, a file, a link, not
//! readable) fails `open` before anything is looked at, whatever else the path holds.
//!
//! Paths are handled as Node's `path.posix` does, lexically and on bytes: the root is resolved
//! against the current directory, the relative path joined and normalised, and a target that
//! does not land strictly below the root is refused.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Write as _};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use js_json::JValue;
use rustix::fs::{AtFlags, FileType, Gid, Mode, OFlags, Uid};
use rustix::io::Errno;

use crate::{permission_json, pin_files_for, Base, PinOptions, Refusal};

/// The user the harness runs as; new directories and files are handed to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

impl Owner {
    // `u32::MAX` is -1, "leave unchanged", as for Node's `fchownSync`.
    fn ids(self) -> (Option<Uid>, Option<Gid>) {
        (
            Some(Uid::from_raw_unchecked(self.uid)),
            Some(Gid::from_raw_unchecked(self.gid)),
        )
    }
}

/// How the written file and the directories made for it are handed to the harness user. `path`
/// is the lexical full path of the descriptor's object (for logging and tests); the change itself
/// must go through the descriptor, never the path.
pub trait Handover {
    /// A directory this call made (opened `O_RDONLY|O_DIRECTORY|O_NOFOLLOW` when it was made).
    /// Called after [`Handover::file`], innermost directory first.
    fn directory(&mut self, dir: BorrowedFd<'_>, path: &Path, owner: Owner) -> io::Result<()>;
    /// The file just written, still open. Called first.
    fn file(&mut self, file: BorrowedFd<'_>, path: &Path, owner: Owner) -> io::Result<()>;
    /// Called just before each directory component or the file is opened, after its check (and,
    /// for a missing directory, its `mkdir`): an observation point for tests of that window. The
    /// default does nothing.
    fn opening(&mut self, _path: &Path) {}
}

/// The production hand-over: `fchown` on the descriptor of the new directory or the open file.
/// Neither names a path, so neither can follow a link or reach a hard link swapped in.
#[derive(Debug, Clone, Copy, Default)]
pub struct Chown;

impl Handover for Chown {
    fn directory(&mut self, dir: BorrowedFd<'_>, _path: &Path, owner: Owner) -> io::Result<()> {
        let (uid, gid) = owner.ids();
        rustix::fs::fchown(dir, uid, gid).map_err(Into::into)
    }

    fn file(&mut self, file: BorrowedFd<'_>, _path: &Path, owner: Owner) -> io::Result<()> {
        let (uid, gid) = owner.ids();
        rustix::fs::fchown(file, uid, gid).map_err(Into::into)
    }
}

/// Why a write did not happen.
#[derive(Debug)]
pub enum PinError {
    /// A refusal with status 409: from [`pin_files_for`], a missing root, or a link or wrong file
    /// type on the way down.
    Refused(Refusal),
    /// The target is not strictly below its root (Node's plain `Error('Pinned file outside its
    /// root')`, no status).
    Outside,
    /// A path component that cannot name an entry of a directory: empty (a relative path that
    /// collapses to the root), `.`, `..` or containing a NUL. Node's plain
    /// `Error('not a single path component')`, no status.
    NotAComponent,
    /// A filesystem call failed (Node rethrows its `fs` error). `syscall` is Node's name for it.
    Io {
        syscall: &'static str,
        path: Option<PathBuf>,
        source: io::Error,
    },
}

impl PinError {
    /// Node's `error.status`.
    pub fn status(&self) -> Option<u16> {
        match self {
            PinError::Refused(r) => Some(r.status),
            _ => None,
        }
    }
}

impl std::fmt::Display for PinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PinError::Refused(r) => f.write_str(&r.message),
            PinError::Outside => f.write_str("Pinned file outside its root"),
            PinError::NotAComponent => f.write_str("not a single path component"),
            PinError::Io {
                syscall,
                path: Some(p),
                source,
            } => write!(f, "{source}, {syscall} '{}'", p.display()),
            PinError::Io {
                syscall,
                path: None,
                source,
            } => write!(f, "{source}, {syscall}"),
        }
    }
}

impl std::error::Error for PinError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PinError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<Refusal> for PinError {
    fn from(r: Refusal) -> Self {
        PinError::Refused(r)
    }
}

/// What the job event records (`writeHarnessConfig`'s return).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The first pinned file's relative path.
    pub file: &'static str,
    /// Every pinned file, `./path` in the working tree or `~/path` in the home.
    pub files: Vec<String>,
    pub model: String,
    pub endpoint: String,
}

impl Record {
    /// `{ file, files, permission, model, endpoint }`, in Node's key order.
    pub fn to_json(&self) -> JValue {
        JValue::obj([
            ("file", JValue::from(self.file)),
            (
                "files",
                JValue::Arr(
                    self.files
                        .iter()
                        .map(|f| JValue::from(f.as_str()))
                        .collect(),
                ),
            ),
            ("permission", permission_json()),
            ("model", JValue::from(self.model.as_str())),
            ("endpoint", JValue::from(self.endpoint.as_str())),
        ])
    }
}

/// Where and as whom to write a harness's files.
#[derive(Clone, Copy, Default)]
pub struct WriteRequest<'a> {
    pub cwd: Option<&'a Path>,
    pub home: Option<&'a Path>,
    pub owner: Option<Owner>,
    pub options: PinOptions<'a>,
}

impl std::fmt::Debug for WriteRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteRequest")
            .field("cwd", &self.cwd)
            .field("home", &self.home)
            .field("owner", &self.owner)
            .field("options", &self.options)
            .finish()
    }
}

fn present(p: Option<&Path>) -> Option<&Path> {
    p.filter(|p| !p.as_os_str().is_empty())
}

/// Write every file [`pin_files_for`] gives, in order, under the working directory or the home,
/// handing each new directory and file to `owner` through `handover` (pass [`Chown`] in
/// production). A refusal from [`pin_files_for`] writes nothing; a later file's failure leaves the
/// earlier files written, as in Node. A file whose root is missing (or empty) is refused with
/// Node's "needs a private home directory" message, whichever root it is.
pub fn write_harness_config(
    req: &WriteRequest<'_>,
    handover: &mut dyn Handover,
) -> Result<Record, PinError> {
    let pinned = pin_files_for(&req.options)?;
    for file in &pinned.files {
        let root = match file.base {
            Base::Cwd => present(req.cwd),
            Base::Home => present(req.home),
        };
        let Some(root) = root else {
            return Err(PinError::Refused(Refusal::conflict(format!(
                "The {} harness needs a private home directory for its configuration.",
                pinned.harness
            ))));
        };
        write_pinned(root, file.path, &file.content, req.owner, handover)?;
    }
    let first = pinned.files.first().map_or("", |f| f.path);
    Ok(Record {
        file: first,
        files: pinned
            .files
            .iter()
            .map(|f| {
                format!(
                    "{}/{}",
                    if f.base == Base::Home { "~" } else { "." },
                    f.path
                )
            })
            .collect(),
        model: pinned.model,
        endpoint: pinned.endpoint,
    })
}

fn refuse(why: String) -> PinError {
    PinError::Refused(Refusal::conflict(format!(
        "Refused to pin the harness configuration: {why}."
    )))
}

fn io_err(syscall: &'static str, path: Option<&[u8]>, source: impl Into<io::Error>) -> PinError {
    PinError::Io {
        syscall,
        path: path.map(|p| PathBuf::from(OsStr::from_bytes(p))),
        source: source.into(),
    }
}

fn as_path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

/// Node's `normalizeString` for an absolute path: segments without `.`, `..` popping (never
/// above the root).
fn segments(path: &[u8]) -> Vec<&[u8]> {
    let mut out: Vec<&[u8]> = Vec::new();
    for seg in path.split(|b| *b == b'/') {
        match seg {
            b"" | b"." => {}
            b".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out
}

fn absolute(segs: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for s in segs {
        out.push(b'/');
        out.extend_from_slice(s);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

/// `path.resolve(root)`: absolute, normalised, no trailing slash.
fn resolve(root: &[u8]) -> Result<Vec<u8>, PinError> {
    if root.first() == Some(&b'/') {
        return Ok(absolute(&segments(root)));
    }
    let cwd = std::env::current_dir().map_err(|e| io_err("uv_cwd", None, e))?;
    let mut full = cwd.as_os_str().as_bytes().to_vec();
    if !root.is_empty() {
        full.push(b'/');
        full.extend_from_slice(root);
    }
    Ok(absolute(&segments(&full)))
}

/// The flags of every directory open: Node's `O_RDONLY | O_DIRECTORY | O_NOFOLLOW`.
fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// `lstat` relative to a directory: `None` when missing.
fn lstat(dir: &OwnedFd, name: &OsStr) -> Result<Option<rustix::fs::Stat>, Errno> {
    match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => Ok(Some(st)),
        Err(Errno::NOENT) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Node's `at()` accepts one component only.
fn component(name: &[u8]) -> Result<&OsStr, PinError> {
    if name.is_empty() || name.contains(&0) || name == b"." || name == b".." {
        return Err(PinError::NotAComponent);
    }
    Ok(OsStr::from_bytes(name))
}

/// Write one pinned file under `root` (`writePinned`). The root is opened without following a
/// link. For each directory between the root and the file, in order: a link is refused, anything
/// not a directory is refused, a missing one is made (mode `0o700`), and the directory is entered
/// through the descriptor of its parent (refused if it is not the one that was checked). Then a
/// link or directory at the target is refused, an existing file is unlinked, and the file is
/// created fresh (mode `0o600`), written and handed over, and then the directories made here are
/// handed over, innermost first. Messages name the directory relative to the root, and the target
/// as `relative` was given. See the module doc.
pub fn write_pinned(
    root: &Path,
    relative: &str,
    content: &str,
    owner: Option<Owner>,
    handover: &mut dyn Handover,
) -> Result<(), PinError> {
    let base = resolve(root.as_os_str().as_bytes())?;
    // path.join(base, relative), normalised; a trailing slash is kept for the containment check
    // only, as Node's `path.relative` then drops it.
    let mut joined = base.clone();
    if !relative.is_empty() {
        joined.push(b'/');
        joined.extend_from_slice(relative.as_bytes());
    }
    let segs = segments(&joined);
    let trailing = joined.last() == Some(&b'/') && !segs.is_empty();
    let target = absolute(&segs);
    let mut checked = target.clone();
    if trailing {
        checked.push(b'/');
    }
    let mut prefix = base.clone();
    prefix.push(b'/');
    if !checked.starts_with(&prefix) {
        return Err(PinError::Outside);
    }
    let parts = segs.get(segments(&base).len()..).unwrap_or_default();
    // No segment below the root: only a path that collapses to it with a trailing slash (`a/../`)
    // gets here; Node's `relative` is then '' and its leaf cannot be spelled.
    let (leaf, dirs): (&[u8], &[&[u8]]) = match parts.split_last() {
        Some((leaf, dirs)) => (leaf, dirs),
        None => (b"", parts),
    };

    let mut dir_fd = rustix::fs::open(as_path(&base), dir_flags(), Mode::empty())
        .map_err(|e| io_err("open", Some(&base), e))?;

    let mut made: Vec<(OwnedFd, PathBuf)> = Vec::new();
    let mut dir = base.clone();
    for (i, part) in dirs.iter().enumerate() {
        let name = component(part)?;
        dir.push(b'/');
        dir.extend_from_slice(part);
        let shown = || {
            let rel: Vec<&[u8]> = parts.iter().take(i + 1).copied().collect();
            String::from_utf8_lossy(&rel.join(&b'/')).into_owned()
        };
        let seen = match lstat(&dir_fd, name) {
            Ok(Some(st)) => match FileType::from_raw_mode(st.st_mode) {
                FileType::Symlink => return Err(refuse(format!("{} is a symbolic link", shown()))),
                FileType::Directory => Some(st),
                _ => return Err(refuse(format!("{} is not a directory", shown()))),
            },
            Ok(None) => None,
            Err(e) => return Err(io_err("lstat", Some(&dir), e)),
        };
        if seen.is_none() {
            rustix::fs::mkdirat(&dir_fd, name, Mode::RWXU)
                .map_err(|e| io_err("mkdir", Some(&dir), e))?;
        }
        handover.opening(as_path(&dir));
        let replaced = || {
            refuse(format!(
                "{} it was replaced while it was being checked",
                shown()
            ))
        };
        let child = match rustix::fs::openat(&dir_fd, name, dir_flags(), Mode::empty()) {
            Ok(fd) => fd,
            // Checked as a directory (or made) a moment ago, a link or something else now.
            Err(Errno::LOOP | Errno::NOTDIR) => return Err(replaced()),
            Err(e) => return Err(io_err("open", Some(&dir), e)),
        };
        let now = rustix::fs::fstat(&child).map_err(|e| io_err("fstat", Some(&dir), e))?;
        let same = seen.is_none_or(|st| st.st_dev == now.st_dev && st.st_ino == now.st_ino);
        if FileType::from_raw_mode(now.st_mode) != FileType::Directory || !same {
            return Err(replaced());
        }
        if seen.is_none() {
            let held = child
                .try_clone()
                .map_err(|e| io_err("open", Some(&dir), e))?;
            made.push((held, PathBuf::from(OsStr::from_bytes(&dir))));
        }
        dir_fd = child;
    }

    let leaf_name = component(leaf)?;
    match lstat(&dir_fd, leaf_name) {
        Ok(Some(st)) => match FileType::from_raw_mode(st.st_mode) {
            FileType::Symlink => return Err(refuse(format!("{relative} is a symbolic link"))),
            FileType::Directory => return Err(refuse(format!("{relative} is a directory"))),
            _ => match rustix::fs::unlinkat(&dir_fd, leaf_name, AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(e) => return Err(io_err("unlink", Some(&target), e)),
            },
        },
        Ok(None) => {}
        Err(e) => return Err(io_err("lstat", Some(&target), e)),
    }
    handover.opening(as_path(&target));
    let fd = rustix::fs::openat(
        &dir_fd,
        leaf_name,
        OFlags::WRONLY
            | OFlags::CREATE
            | OFlags::EXCL
            | OFlags::TRUNC
            | OFlags::NOFOLLOW
            | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|e| io_err("open", Some(&target), e))?;
    let mut file = File::from(fd);
    file.write_all(content.as_bytes())
        .map_err(|e| io_err("write", None, e))?;
    if let Some(o) = owner {
        handover
            .file(file.as_fd(), as_path(&target), o)
            .map_err(|e| io_err("fchown", Some(&target), e))?;
    }
    drop(file);
    if let Some(o) = owner {
        for (fd, path) in made.iter().rev() {
            handover
                .directory(fd.as_fd(), path, o)
                .map_err(|e| io_err("fchown", Some(path.as_os_str().as_bytes()), e))?;
        }
    }
    Ok(())
}
