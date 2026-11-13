//! Writing the pinned files (`writeHarnessConfig`, `writePinned`).
//!
//! The repository is not noevia's: a checkout can carry a symlink named `.claude` or
//! `opencode.json`, and noevia runs as root, so a write through such a link would put the engine
//! key wherever the repository pointed and hand it to the harness user. Nothing below the root
//! may be a link, the target is replaced rather than written in place, and the final create is
//! `O_CREAT|O_EXCL`, which never follows a link even if one appeared in between (Node also passes
//! `O_NOFOLLOW|O_TRUNC`; with `O_EXCL` neither changes the outcome). The root itself is the
//! caller's (the task's worktree or private home) and is used as given.
//!
//! Paths are handled as Node's `path.posix` does, lexically and on bytes: the root is resolved
//! against the current directory, the relative path joined and normalised, and a target that
//! does not land strictly below the root is refused.

use std::ffi::OsStr;
use std::io::{self, Write as _};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use js_json::JValue;

use crate::{permission_json, pin_files_for, Base, PinOptions, Refusal};

/// The user the harness runs as; new directories and files are handed to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
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
#[derive(Debug, Clone, Copy, Default)]
pub struct WriteRequest<'a> {
    pub cwd: Option<&'a Path>,
    pub home: Option<&'a Path>,
    pub owner: Option<Owner>,
    pub options: PinOptions<'a>,
}

/// The real hand-over: `lchown(2)`, never following a link.
pub fn lchown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))
}

fn present(p: Option<&Path>) -> Option<&Path> {
    p.filter(|p| !p.as_os_str().is_empty())
}

/// Write every file [`pin_files_for`] gives, in order, under the working directory or the home,
/// handing each new directory and file to `owner` through `chown` (pass [`lchown`] in production).
/// A refusal from [`pin_files_for`] writes nothing; a later file's failure leaves the earlier
/// files written, as in Node. A file whose root is missing (or empty) is refused with Node's
/// "needs a private home directory" message, whichever root it is.
pub fn write_harness_config(
    req: &WriteRequest<'_>,
    chown: &mut dyn FnMut(&Path, u32, u32) -> io::Result<()>,
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
        write_pinned(root, file.path, &file.content, req.owner, chown)?;
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

fn io_err(syscall: &'static str, path: Option<&[u8]>, source: io::Error) -> PinError {
    PinError::Io {
        syscall,
        path: path.map(|p| PathBuf::from(OsStr::from_bytes(p))),
        source,
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

/// Write one pinned file under `root` (`writePinned`). For each directory between the root and the
/// file, in order: a link is refused, anything not a directory is refused, a missing one is made
/// (mode `0o700`) and handed over. Then a link or directory at the target is refused, an existing
/// file is unlinked, and the file is created fresh (mode `0o600`), written and handed over.
/// Messages name the directory relative to the root, and the target as `relative` was given.
pub fn write_pinned(
    root: &Path,
    relative: &str,
    content: &str,
    owner: Option<Owner>,
    chown: &mut dyn FnMut(&Path, u32, u32) -> io::Result<()>,
) -> Result<(), PinError> {
    let base = resolve(root.as_os_str().as_bytes())?;
    // path.join(base, relative), normalised; a trailing slash is kept, as Node keeps it.
    let mut joined = base.clone();
    if !relative.is_empty() {
        joined.push(b'/');
        joined.extend_from_slice(relative.as_bytes());
    }
    let segs = segments(&joined);
    let mut target = absolute(&segs);
    if joined.last() == Some(&b'/') && !segs.is_empty() {
        target.push(b'/');
    }
    let mut prefix = base.clone();
    prefix.push(b'/');
    if !target.starts_with(&prefix) {
        return Err(PinError::Outside);
    }
    let base_segs = segments(&base).len();
    let parts = segs.get(base_segs..).unwrap_or_default();
    let dirs = parts.len().saturating_sub(1);
    let mut dir = base.clone();
    for (i, part) in parts.iter().take(dirs).enumerate() {
        dir.push(b'/');
        dir.extend_from_slice(part);
        let shown = || {
            let rel: Vec<&[u8]> = parts.iter().take(i + 1).copied().collect();
            String::from_utf8_lossy(&rel.join(&b'/')).into_owned()
        };
        let path = as_path(&dir);
        match std::fs::symlink_metadata(path) {
            Ok(st) if st.file_type().is_symlink() => {
                return Err(refuse(format!("{} is a symbolic link", shown())))
            }
            Ok(st) if !st.is_dir() => {
                return Err(refuse(format!("{} is not a directory", shown())))
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                std::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(path)
                    .map_err(|e| io_err("mkdir", Some(&dir), e))?;
                if let Some(o) = owner {
                    chown(path, o.uid, o.gid).map_err(|e| io_err("lchown", Some(&dir), e))?;
                }
            }
            Err(e) => return Err(io_err("lstat", Some(&dir), e)),
        }
    }
    let path = as_path(&target);
    match std::fs::symlink_metadata(path) {
        Ok(st) if st.file_type().is_symlink() => {
            return Err(refuse(format!("{relative} is a symbolic link")))
        }
        Ok(st) if st.is_dir() => return Err(refuse(format!("{relative} is a directory"))),
        Ok(_) => std::fs::remove_file(path).map_err(|e| io_err("unlink", Some(&target), e))?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err("lstat", Some(&target), e)),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| io_err("open", Some(&target), e))?;
    file.write_all(content.as_bytes())
        .map_err(|e| io_err("write", None, e))?;
    drop(file);
    if let Some(o) = owner {
        chown(path, o.uid, o.gid).map_err(|e| io_err("lchown", Some(&target), e))?;
    }
    Ok(())
}
