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
//! **Differences from Node** (tested): every open happens beneath the worktree root through a
//! directory handle (`openat2` with `RESOLVE_BENEATH`, the file with `O_NOFOLLOW|O_NONBLOCK`) and
//! new directories are made one at a time with `mkdirat` and entered with `O_NOFOLLOW`, so a
//! harness racing the checks cannot make noevia create a file or directory outside the tree
//! (Node checked by path, then opened and made directories by path: sbstndalton/noevia#2548),
//! and a FIFO is refused as "Not a file" instead of blocking the request; a read returns at most
//! `max` bytes even if the file grew after it was measured (Node read to the end); a lone
//! surrogate in written text is written as U+FFFD, as Node's UTF-8 encoding does, from the
//! `wtf16` form text arrives in; a relative target is refused (the session always passes the
//! absolute path it judged; Node resolved one against the process's directory).

use js_json::wtf16;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;

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
    match trimmed.rfind('/') {
        None | Some(0) => "/".into(),
        Some(i) => trimmed.get(..i).unwrap_or("/").to_string(),
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

/// code-harness.cjs `pinnedParent`: the checked final path, and the root it was judged against.
fn pinned_parent(target: &str, root: Option<&str>) -> Result<(String, String), FileRefusal> {
    if !target.starts_with('/') {
        return Err(refuse(OUTSIDE));
    }
    let root = root
        .filter(|r| !r.is_empty())
        .ok_or_else(|| refuse(OUTSIDE))?;
    let real_root = realpath(root).ok_or_else(|| refuse(OUTSIDE))?;
    let real_parent = realpath(&dirname(target)).ok_or_else(|| refuse(OUTSIDE))?;
    if outside(&relative(&real_root, &real_parent)) {
        return Err(refuse(OUTSIDE));
    }
    let last = join(&real_parent, &basename(target));
    match fs::symlink_metadata(&last) {
        Ok(m) if m.file_type().is_symlink() => Err(refuse(SYMLINK)),
        Ok(_) => Ok((last, real_root)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((last, real_root)),
        Err(e) => Err(os(&e)),
    }
}

/// code-harness.cjs `verifyOpened`: the descriptor is the file `last` names now, still inside.
fn verify_opened(
    file: &fs::File,
    last: &str,
    root: Option<&str>,
) -> Result<fs::Metadata, FileRefusal> {
    let opened = file.metadata().map_err(|e| os(&e))?;
    let root = root.unwrap_or("");
    let real_root = realpath(root).ok_or_else(|| refuse(OUTSIDE))?;
    let real_parent = realpath(&dirname(last)).ok_or_else(|| refuse(OUTSIDE))?;
    let named =
        fs::symlink_metadata(join(&real_parent, &basename(last))).map_err(|_| refuse(OUTSIDE))?;
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

fn errno(e: rustix::io::Errno) -> FileRefusal {
    match e {
        rustix::io::Errno::LOOP => refuse(SYMLINK),
        // Resolution would have left the worktree (a component swapped for a link since the check).
        rustix::io::Errno::XDEV => refuse(OUTSIDE),
        other => os(&std::io::Error::from(other)),
    }
}

/// The worktree root, opened as a directory handle every later open is resolved beneath.
fn root_fd(real_root: &str) -> Result<rustix::fd::OwnedFd, FileRefusal> {
    use rustix::fs::{open, Mode, OFlags};
    open(
        real_root,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| refuse(OUTSIDE))
}

/// Opens `last` (an absolute path under `real_root`) beneath the root: no `..`, no link may lead
/// out of it whatever was swapped in since the path checks, the final component is never a link,
/// and a FIFO does not block the open.
fn open_beneath(
    real_root: &str,
    last: &str,
    flags: rustix::fs::OFlags,
    mode: u32,
) -> Result<fs::File, FileRefusal> {
    use rustix::fs::{openat2, Mode, OFlags, ResolveFlags};
    let rel = relative(real_root, last);
    if rel.is_empty() || outside(&rel) {
        return Err(refuse(OUTSIDE));
    }
    let root = root_fd(real_root)?;
    let fd = openat2(
        &root,
        rel.as_str(),
        flags | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::from_raw_mode(mode),
        ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(errno)?;
    Ok(fs::File::from(fd))
}

/// `defaultFiles.read(target, max, root)`: the file's text (UTF-8, decoded lossily as Node does).
pub fn read(target: &str, max: u64, root: Option<&str>) -> Result<String, FileRefusal> {
    let (last, real_root) = pinned_parent(target, root)?;
    let mut file = open_beneath(&real_root, &last, rustix::fs::OFlags::RDONLY, 0)?;
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

/// Makes `dir`'s missing directories from `existing` down, each with `mkdirat` beneath the root
/// and entered with `O_NOFOLLOW`, so a component swapped for a link leads nowhere.
fn make_beneath(real_root: &str, existing: &str, dir: &str) -> Result<(), FileRefusal> {
    use rustix::fs::{mkdirat, openat, openat2, Mode, OFlags, ResolveFlags};
    let dflags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let root = root_fd(real_root)?;
    let real_existing = realpath(existing).ok_or_else(|| refuse(OUTSIDE))?;
    let from = relative(real_root, &real_existing);
    if outside(&from) {
        return Err(refuse(OUTSIDE));
    }
    let mut fd = if from.is_empty() {
        root
    } else {
        openat2(
            &root,
            from.as_str(),
            dflags | OFlags::NOFOLLOW,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS,
        )
        .map_err(errno)?
    };
    let rest = relative(existing, dir);
    for part in rest.split('/').filter(|p| !p.is_empty()) {
        if part == ".." {
            return Err(refuse(OUTSIDE));
        }
        match mkdirat(&fd, part, Mode::from_raw_mode(0o777)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(errno(e)),
        }
        fd = openat(&fd, part, dflags | OFlags::NOFOLLOW, Mode::empty()).map_err(|e| match e {
            rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP => refuse(OUTSIDE),
            other => errno(other),
        })?;
    }
    Ok(())
}

/// `defaultFiles.write(target, text, root)`: missing directories made first (only under a real
/// directory inside the root), then the checked file truncated and written.
pub fn write(target: &str, text: &str, root: Option<&str>) -> Result<(), FileRefusal> {
    if !target.starts_with('/') {
        return Err(refuse(OUTSIDE));
    }
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
            make_beneath(&real_root, &existing, &dir)?;
        }
    }
    let (last, real_root) = pinned_parent(target, root)?;
    // Not truncated on open: nothing changes until the descriptor is proven to be the checked file.
    let mut file = open_beneath(
        &real_root,
        &last,
        rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE,
        0o644,
    )?;
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
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use std::path::Path;

    fn tree() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path())
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let root = format!("{base}/tree");
        fs::create_dir_all(format!("{root}/sub")).unwrap();
        fs::create_dir_all(format!("{base}/outside")).unwrap();
        (dir, base, root)
    }

    #[test]
    fn a_fifo_is_not_a_file_and_never_blocks() {
        let (_d, _base, root) = tree();
        let fifo = format!("{root}/p");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            fifo.as_str(),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o644),
            0,
        )
        .unwrap();
        let t = std::time::Instant::now();
        assert_eq!(
            read(&fifo, 100, Some(&root)).unwrap_err().message,
            "Not a file"
        );
        // No reader: the non-blocking write open fails at once (ENXIO), never hangs.
        assert!(write(&fifo, "x", Some(&root)).is_err());
        assert!(t.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn a_parent_swapped_for_a_link_after_the_checks_leads_nowhere() {
        let (_d, base, root) = tree();
        // The checks saw `sub` as a directory inside; the harness then made it a link out.
        fs::remove_dir(format!("{root}/sub")).unwrap();
        std::os::unix::fs::symlink(format!("{base}/outside"), format!("{root}/sub")).unwrap();
        let flags = rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::CREATE;
        let e = open_beneath(&root, &format!("{root}/sub/nologin"), flags, 0o644).unwrap_err();
        assert_eq!(e.message, OUTSIDE);
        assert!(!Path::new(&format!("{base}/outside/nologin")).exists());
        // The opened path is built from the realpath, so it holds no link; one swapped in later
        // is followed only while it stays beneath the root (a relative one), never an absolute one.
        fs::create_dir_all(format!("{root}/real")).unwrap();
        std::os::unix::fs::symlink("real", format!("{root}/inlink")).unwrap();
        std::os::unix::fs::symlink(format!("{root}/real"), format!("{root}/abslink")).unwrap();
        let e = open_beneath(&root, &format!("{root}/abslink/x.txt"), flags, 0o644).unwrap_err();
        assert_eq!(e.message, OUTSIDE);
        open_beneath(&root, &format!("{root}/inlink/ok.txt"), flags, 0o644).unwrap();
        assert!(Path::new(&format!("{root}/real/ok.txt")).exists());
        // The final component is never a link, wherever it points.
        let e = open_beneath(
            &root,
            &format!("{root}/inlink"),
            rustix::fs::OFlags::RDONLY,
            0,
        )
        .unwrap_err();
        assert_eq!(e.message, SYMLINK);
    }

    #[test]
    fn new_directories_are_never_made_through_a_link() {
        let (_d, base, root) = tree();
        // `new` turned into a link out between the walk's check and the making.
        std::os::unix::fs::symlink(format!("{base}/outside"), format!("{root}/new")).unwrap();
        let e = make_beneath(&root, &root, &format!("{root}/new/a/b")).unwrap_err();
        assert_eq!(e.message, OUTSIDE);
        assert!(!Path::new(&format!("{base}/outside/a")).exists());
        make_beneath(&root, &root, &format!("{root}/fresh/a/b")).unwrap();
        assert!(Path::new(&format!("{root}/fresh/a/b")).is_dir());
    }

    #[test]
    fn a_relative_target_is_refused() {
        let (_d, _base, root) = tree();
        assert_eq!(read("sub/x", 10, Some(&root)).unwrap_err().message, OUTSIDE);
        assert_eq!(
            write("x.txt", "x", Some(&root)).unwrap_err().message,
            OUTSIDE
        );
    }
}
