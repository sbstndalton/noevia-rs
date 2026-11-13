//! core code-harness.cjs `defaultFiles`: the only way a Code agent reads or writes a file through
//! noevia (ACP `fs/read_text_file`, `fs/write_text_file`), always inside the task's worktree.
//! The parent directory must realpath inside the worktree root, the final component must not be
//! a symlink (dangling or not, pointing inside or out), the file is opened with `O_NOFOLLOW`, and
//! after opening the descriptor must be the very file the checked path names now (same device and
//! inode, its parent still inside the root) and not one with a second hard link. A write first
//! judges the deepest directory that already exists, so a missing directory is created only under
//! a real directory inside the root. Node's algorithm and its quirks are kept (an in-tree link to
//! a directory may be written through, but never have directories made under it), proven by the
//! fixture recorded from Node (tests/files.rs).
//!
//! **Differences from Node** (tested): a read returns at most `max` bytes even if the file grew
//! after it was measured (Node read to the end); a lone surrogate in written text is written as
//! U+FFFD, as Node's UTF-8 encoding does, from the `wtf16` form text arrives in.

use js_json::wtf16;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

/// The JSON-RPC "invalid params" code a refusal carries to the agent.
pub const INVALID_PARAMS: i64 = -32602;
const OUTSIDE: &str = "Outside this task\u{2019}s workspace";
const SYMLINK: &str = "Refusing to follow a symlink";
const HARD_LINK: &str = "Refusing a file with more than one hard link";

/// A refused read or write: `code` [`INVALID_PARAMS`] for noevia's own refusals (the agent is told
/// why), `None` for an operating-system failure (answered as internal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRefusal {
    pub code: Option<i64>,
    pub message: String,
}

fn refuse(message: &str) -> FileRefusal {
    FileRefusal {
        code: Some(INVALID_PARAMS),
        message: message.to_string(),
    }
}

fn os(e: &std::io::Error) -> FileRefusal {
    FileRefusal {
        code: None,
        message: e.to_string(),
    }
}

/// `path.dirname` for an absolute path.
fn dirname(p: &str) -> String {
    let trimmed = p.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into();
    }
    match trimmed.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => trimmed.get(..i).unwrap_or("/").trim_end_matches('/').to_string(),
    }
    .chars()
    .collect::<String>()
    .pipe_root()
}

trait PipeRoot {
    fn pipe_root(self) -> String;
}

impl PipeRoot for String {
    fn pipe_root(self) -> String {
        if self.is_empty() {
            "/".into()
        } else {
            self
        }
    }
}

/// `path.basename` for an absolute path.
fn basename(p: &str) -> String {
    let trimmed = p.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or("").to_string()
}

/// `path.resolve` of an absolute path: `.` and `..` folded, no trailing slash.
fn resolve(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

fn join(dir: &str, name: &str) -> String {
    resolve(&format!("{dir}/{name}"))
}

/// `path.relative(from, to)` for absolute paths.
fn relative(from: &str, to: &str) -> String {
    let (from, to) = (resolve(from), resolve(to));
    let a: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
    let b: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let common = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    let mut out: Vec<&str> = vec![".."; a.len().saturating_sub(common)];
    out.extend(b.iter().skip(common));
    out.join("/")
}

fn realpath(p: &str) -> Option<String> {
    if p.is_empty() {
        return None;
    }
    fs::canonicalize(p)
        .ok()
        .and_then(|r| r.into_os_string().into_string().ok())
}

/// `rel.startsWith('..') || path.isAbsolute(rel)`: not under the root.
fn outside(rel: &str) -> bool {
    rel.starts_with("..") || rel.starts_with('/')
}

/// code-harness.cjs `pinnedParent`: the checked final path.
fn pinned_parent(target: &str, root: Option<&str>) -> Result<String, FileRefusal> {
    let root = root.filter(|r| !r.is_empty()).ok_or_else(|| refuse(OUTSIDE))?;
    let real_root = realpath(root).ok_or_else(|| refuse(OUTSIDE))?;
    let real_parent = realpath(&dirname(target)).ok_or_else(|| refuse(OUTSIDE))?;
    if outside(&relative(&real_root, &real_parent)) {
        return Err(refuse(OUTSIDE));
    }
    let last = join(&real_parent, &basename(target));
    match fs::symlink_metadata(&last) {
        Ok(m) if m.file_type().is_symlink() => Err(refuse(SYMLINK)),
        Ok(_) => Ok(last),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(last),
        Err(e) => Err(os(&e)),
    }
}

/// code-harness.cjs `verifyOpened`: the descriptor is the file `last` names now, still inside.
fn verify_opened(file: &fs::File, last: &str, root: Option<&str>) -> Result<fs::Metadata, FileRefusal> {
    let opened = file.metadata().map_err(|e| os(&e))?;
    let root = root.unwrap_or("");
    let real_root = realpath(root).ok_or_else(|| refuse(OUTSIDE))?;
    let real_parent = realpath(&dirname(last)).ok_or_else(|| refuse(OUTSIDE))?;
    let named = fs::symlink_metadata(join(&real_parent, &basename(last))).map_err(|_| refuse(OUTSIDE))?;
    if outside(&relative(&real_root, &real_parent)) {
        return Err(refuse(OUTSIDE));
    }
    if named.dev() != opened.dev() || named.ino() != opened.ino() {
        return Err(refuse(OUTSIDE));
    }
    if opened.is_file() && opened.nlink() > 1 {
        return Err(refuse(HARD_LINK));
    }
    Ok(opened)
}

fn open_error(e: &std::io::Error) -> FileRefusal {
    if e.raw_os_error() == Some(40) {
        refuse(SYMLINK)
    } else {
        os(e)
    }
}

/// `defaultFiles.read(target, max, root)`: the file's text (UTF-8, decoded lossily as Node does).
pub fn read(target: &str, max: u64, root: Option<&str>) -> Result<String, FileRefusal> {
    let last = pinned_parent(target, root)?;
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc_o_nofollow())
        .open(&last)
        .map_err(|e| open_error(&e))?;
    let stat = verify_opened(&file, &last, root)?;
    if !stat.is_file() {
        return Err(refuse("Not a file"));
    }
    if stat.len() > max {
        return Err(refuse("File too large"));
    }
    let mut raw = Vec::new();
    (&mut file)
        .take(max.saturating_add(1))
        .read_to_end(&mut raw)
        .map_err(|e| os(&e))?;
    // It grew after it was measured: still too large.
    if raw.len() as u64 > max {
        return Err(refuse("File too large"));
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// `defaultFiles.write(target, text, root)`: missing directories made first (only under a real
/// directory inside the root), then the checked file truncated and written.
pub fn write(target: &str, text: &str, root: Option<&str>) -> Result<(), FileRefusal> {
    if let Some(root) = root.filter(|r| !r.is_empty()) {
        let real_root = realpath(root).ok_or_else(|| refuse(OUTSIDE))?;
        let dir = resolve(&dirname(target));
        let mut existing = dir.clone();
        // `fs.existsSync` follows links: a dangling one on the way is refused.
        while fs::metadata(&existing).is_err() {
            if fs::symlink_metadata(&existing).is_ok() {
                return Err(refuse(OUTSIDE));
            }
            let up = dirname(&existing);
            if up == existing {
                return Err(refuse(OUTSIDE));
            }
            existing = up;
        }
        if existing != dir
            && fs::symlink_metadata(&existing)
                .map(|m| m.file_type().is_symlink())
                .map_err(|e| os(&e))?
        {
            return Err(refuse(OUTSIDE));
        }
        let real_existing = fs::canonicalize(&existing)
            .map_err(|e| os(&e))?
            .into_os_string()
            .into_string()
            .map_err(|_| refuse(OUTSIDE))?;
        if outside(&relative(&real_root, &real_existing)) {
            return Err(refuse(OUTSIDE));
        }
        if existing != dir {
            fs::create_dir_all(&dir).map_err(|e| os(&e))?;
        }
    }
    let last = pinned_parent(target, root)?;
    // Not truncated on open: nothing changes until the descriptor is proven to be the checked file.
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o644)
        .custom_flags(libc_o_nofollow())
        .open(&last)
        .map_err(|e| open_error(&e))?;
    let stat = verify_opened(&file, &last, root)?;
    if !stat.is_file() {
        return Err(refuse("Not a file"));
    }
    file.set_len(0).map_err(|e| os(&e))?;
    file.write_all(utf8_of(text).as_bytes()).map_err(|e| os(&e))
}

/// The bytes Node's UTF-8 encoding gives a JS string: a lone surrogate (held as its mapped
/// character in the `wtf16` form) becomes U+FFFD.
pub fn utf8_of(text: &str) -> String {
    text.chars()
        .map(|c| if wtf16::lone_unit(c).is_some() { '\u{FFFD}' } else { c })
        .collect()
}

/// `O_NOFOLLOW` (Linux).
fn libc_o_nofollow() -> i32 {
    rustix::fs::OFlags::NOFOLLOW.bits() as i32
}
