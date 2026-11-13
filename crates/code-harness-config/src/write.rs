//! Writing the pinned files (`writeHarnessConfig`, `writePinned`).
//!
//! The repository is not noevia's: a checkout can carry a symlink named `.claude` or
//! `opencode.json`, and noevia runs as root, so a write through such a link would put the engine
//! key wherever the repository pointed and hand it to the harness user. Nothing below the root
//! may be a link, and the target is replaced rather than written in place.
//!
//! Named difference from Node (`fd-walk`): Node checks each component by path (`lstat`) and then
//! unlinks, opens and `lchown`s by the full path again, so a link swapped in between (the
//! harness user owns the tree, and each new directory is handed to it before the walk descends)
//! is followed. Here the walk holds a descriptor per directory: the root is opened (followed: it
//! is the caller's), and each component is classified with `fstatat(AT_SYMLINK_NOFOLLOW)` (the
//! checks and messages Node has), made with `mkdirat` when missing and entered with
//! `openat(O_DIRECTORY|O_NOFOLLOW)`, all relative to the parent descriptor; a directory just made
//! is handed over with `fchown` on that descriptor before the walk descends through it. The old file is removed with `unlinkat`, the new one created with
//! `openat(O_CREAT|O_EXCL|O_NOFOLLOW, 0o600)` and handed over with `fchown` on its descriptor
//! before it is closed. A link swapped in after the check, which only the descriptor walk sees,
//! is refused with Node's "is a symbolic link" message. Node's order of checks, its messages, its
//! modes and the order of hand-over calls are otherwise unchanged (the recorded fixture).
//!
//! When the root itself cannot be opened, the error is reported as the call Node would have
//! failed on: `mkdir` (missing root, a directory to make) or `open` (missing root, file directly
//! under it) for ENOENT, `lstat` otherwise.
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
    // `u32::MAX` is -1, "leave unchanged", as for Node's `lchownSync`.
    fn ids(self) -> (Option<Uid>, Option<Gid>) {
        (
            Some(Uid::from_raw_unchecked(self.uid)),
            Some(Gid::from_raw_unchecked(self.gid)),
        )
    }
}

/// How new directories and the written file are handed to the harness user. `path` is the full
/// path Node would pass to `lchownSync` (for logging and tests); the change itself must go through
/// the descriptors, never the path.
pub trait Handover {
    /// A directory just made, opened (`O_DIRECTORY|O_NOFOLLOW`) before it is handed over and
    /// before the walk descends through it.
    fn directory(&mut self, dir: BorrowedFd<'_>, path: &Path, owner: Owner) -> io::Result<()>;
    /// The file just written, still open.
    fn file(&mut self, file: BorrowedFd<'_>, path: &Path, owner: Owner) -> io::Result<()>;
    /// Called just before each directory component or the file is opened, after its check: an
    /// observation point for tests of that window. The default does nothing.
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

/// Directory descriptors: search-only where the platform has `O_PATH` (Node needs only search
/// permission on the way down), read-only elsewhere.
fn dir_flags() -> OFlags {
    let base = OFlags::DIRECTORY | OFlags::CLOEXEC;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        base | OFlags::PATH
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        base | OFlags::RDONLY
    }
}

/// `lstat` relative to a directory: `None` when missing.
fn kind(dir: &OwnedFd, name: &OsStr) -> Result<Option<FileType>, Errno> {
    match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => Ok(Some(FileType::from_raw_mode(st.st_mode))),
        Err(Errno::NOENT) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write one pinned file under `root` (`writePinned`). For each directory between the root and the
/// file, in order: a link is refused, anything not a directory is refused, a missing one is made
/// (mode `0o700`) and handed over. Then a link or directory at the target is refused, an existing
/// file is unlinked, and the file is created fresh (mode `0o600`), written and handed over.
/// Messages name the directory relative to the root, and the target as `relative` was given.
/// Every step goes through the parent directory's descriptor (see the module doc).
pub fn write_pinned(
    root: &Path,
    relative: &str,
    content: &str,
    owner: Option<Owner>,
    handover: &mut dyn Handover,
) -> Result<(), PinError> {
    let base = resolve(root.as_os_str().as_bytes())?;
    // path.join(base, relative), normalised; a trailing slash is kept, as Node keeps it.
    let mut joined = base.clone();
    if !relative.is_empty() {
        joined.push(b'/');
        joined.extend_from_slice(relative.as_bytes());
    }
    let segs = segments(&joined);
    let trailing = joined.last() == Some(&b'/') && !segs.is_empty();
    let mut target = absolute(&segs);
    if trailing {
        target.push(b'/');
    }
    let mut prefix = base.clone();
    prefix.push(b'/');
    if !target.starts_with(&prefix) {
        return Err(PinError::Outside);
    }
    let parts = segs.get(segments(&base).len()..).unwrap_or_default();
    // No segment below the root: only a path that collapses to it with a trailing slash (`a/../`)
    // gets here, and Node finds a directory at its target.
    let (leaf, dirs) = match parts.split_last() {
        Some((leaf, dirs)) => (Some(leaf), dirs),
        None => (None, parts),
    };

    let root_flags = dir_flags();
    let mut dir_fd = rustix::fs::open(as_path(&base), root_flags, Mode::empty()).map_err(|e| {
        let first = match dirs.first() {
            Some(part) => [base.as_slice(), b"/", part].concat(),
            None => target.clone(),
        };
        let syscall = match (e, dirs.is_empty()) {
            (Errno::NOENT, false) => "mkdir",
            (Errno::NOENT, true) => "open",
            _ => "lstat",
        };
        io_err(syscall, Some(&first), e)
    })?;
    let Some(leaf) = leaf else {
        return Err(refuse(format!("{relative} is a directory")));
    };

    let mut dir = base.clone();
    for (i, part) in dirs.iter().enumerate() {
        dir.push(b'/');
        dir.extend_from_slice(part);
        let shown = || {
            let rel: Vec<&[u8]> = parts.iter().take(i + 1).copied().collect();
            String::from_utf8_lossy(&rel.join(&b'/')).into_owned()
        };
        let name = OsStr::from_bytes(part);
        let made = match kind(&dir_fd, name) {
            Ok(Some(FileType::Symlink)) => {
                return Err(refuse(format!("{} is a symbolic link", shown())))
            }
            Ok(Some(FileType::Directory)) => false,
            Ok(Some(_)) => return Err(refuse(format!("{} is not a directory", shown()))),
            Ok(None) => {
                rustix::fs::mkdirat(&dir_fd, name, Mode::RWXU)
                    .map_err(|e| io_err("mkdir", Some(&dir), e))?;
                true
            }
            Err(e) => return Err(io_err("lstat", Some(&dir), e)),
        };
        handover.opening(as_path(&dir));
        // A directory just made is ours and readable; it is opened for reading so it can be
        // handed over with `fchown` (which an `O_PATH` descriptor cannot do).
        let flags = if made {
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
        } else {
            root_flags | OFlags::NOFOLLOW
        };
        dir_fd = match rustix::fs::openat(&dir_fd, name, flags, Mode::empty()) {
            Ok(fd) => fd,
            // Checked as a directory (or made) a moment ago, a link or something else now: swapped.
            Err(Errno::LOOP | Errno::NOTDIR) => {
                return Err(refuse(format!("{} is a symbolic link", shown())))
            }
            Err(e) => return Err(io_err("open", Some(&dir), e)),
        };
        if let (true, Some(o)) = (made, owner) {
            handover
                .directory(dir_fd.as_fd(), as_path(&dir), o)
                .map_err(|e| io_err("lchown", Some(&dir), e))?;
        }
    }

    // The last segment as Node passes it: with its trailing slash, if the path had one.
    let leaf_name: Vec<u8> = if trailing {
        [*leaf, b"/"].concat()
    } else {
        leaf.to_vec()
    };
    let leaf_name = OsStr::from_bytes(&leaf_name);
    match kind(&dir_fd, leaf_name) {
        Ok(Some(FileType::Symlink)) => {
            return Err(refuse(format!("{relative} is a symbolic link")))
        }
        Ok(Some(FileType::Directory)) => return Err(refuse(format!("{relative} is a directory"))),
        Ok(Some(_)) => rustix::fs::unlinkat(&dir_fd, leaf_name, AtFlags::empty())
            .map_err(|e| io_err("unlink", Some(&target), e))?,
        Ok(None) => {}
        Err(e) => return Err(io_err("lstat", Some(&target), e)),
    }
    handover.opening(as_path(&target));
    let fd = rustix::fs::openat(
        &dir_fd,
        leaf_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|e| io_err("open", Some(&target), e))?;
    let mut file = File::from(fd);
    file.write_all(content.as_bytes())
        .map_err(|e| io_err("write", None, e))?;
    if let Some(o) = owner {
        handover
            .file(file.as_fd(), as_path(&target), o)
            .map_err(|e| io_err("lchown", Some(&target), e))?;
    }
    Ok(())
}
