//! The coding harness's rooted file API. Nothing is truncated until its open descriptor has
//! been checked against the current path, and hard links and final symlinks are refused.
use crate::acp::RpcError;
use js_json::wtf16;
use std::{
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

pub const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
fn refused(message: &str) -> RpcError {
    RpcError {
        code: -32602,
        message: message.into(),
        data: None,
    }
}
fn outside() -> RpcError {
    refused("Outside this task’s workspace")
}
fn node_text(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if wtf16::lone_unit(c).is_some() {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}
fn resolved(root: &Path, target: &str) -> PathBuf {
    let raw = PathBuf::from(node_text(target));
    let input = if raw.is_absolute() {
        raw
    } else {
        root.join(raw)
    };
    let mut out = PathBuf::new();
    for component in input.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}
fn beneath(root: &Path, path: &Path) -> bool {
    if root == path {
        return true;
    }
    path.strip_prefix(root)
        .ok()
        .and_then(|r| r.components().next())
        .is_some_and(|c| !c.as_os_str().to_string_lossy().starts_with(".."))
}
/// Node workspaces.contains, including its conservative unresolved-ancestor rule.
pub fn contains(root: &Path, candidate: &str) -> bool {
    if candidate.is_empty() {
        return false;
    }
    let target = resolved(root, candidate);
    let Ok(real_root) = fs::canonicalize(root) else {
        return false;
    };
    if fs::symlink_metadata(&target).is_ok_and(|s| s.file_type().is_symlink()) && !target.exists() {
        return false;
    }
    let mut probe = target.clone();
    let mut unresolved = 0;
    let real_probe = loop {
        if let Ok(path) = fs::canonicalize(&probe) {
            break path;
        }
        unresolved += 1;
        if unresolved > 64 || !probe.pop() {
            return false;
        }
    };
    // A lexical relative path back from the resolved probe must not climb through '..'.
    (real_probe == target || target.starts_with(&real_probe)) && beneath(&real_root, &real_probe)
}
fn pinned_parent(target: &Path, root: &Path) -> Result<PathBuf, RpcError> {
    let real_root = fs::canonicalize(root).map_err(|_| outside())?;
    let real_parent =
        fs::canonicalize(target.parent().ok_or_else(outside)?).map_err(|_| outside())?;
    if !beneath(&real_root, &real_parent) {
        return Err(outside());
    }
    let final_path = real_parent.join(target.file_name().ok_or_else(outside)?);
    match fs::symlink_metadata(&final_path) {
        Ok(m) if m.file_type().is_symlink() => return Err(refused("Refusing to follow a symlink")),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_error("lstat", &final_path, e)),
    }
    Ok(final_path)
}
fn verify(file: &fs::File, path: &Path, root: &Path) -> Result<fs::Metadata, RpcError> {
    let opened = file.metadata().map_err(|e| io_error("fstat", path, e))?;
    let real_root = fs::canonicalize(root).map_err(|_| outside())?;
    let real_parent =
        fs::canonicalize(path.parent().ok_or_else(outside)?).map_err(|_| outside())?;
    if !beneath(&real_root, &real_parent) {
        return Err(outside());
    }
    let named = fs::symlink_metadata(real_parent.join(path.file_name().ok_or_else(outside)?))
        .map_err(|_| outside())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != named.dev() || opened.ino() != named.ino() {
            return Err(outside());
        }
        if opened.is_file() && opened.nlink() > 1 {
            return Err(refused("Refusing a file with more than one hard link"));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = named;
        return Err(outside());
    }
    Ok(opened)
}
fn open(path: &Path, write: bool) -> Result<fs::File, RpcError> {
    let mut options = fs::OpenOptions::new();
    if write {
        options.write(true).create(true).truncate(false);
    } else {
        options.read(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options.open(path).map_err(|e| {
        if e.raw_os_error() == Some(libc::ELOOP) {
            refused("Refusing to follow a symlink")
        } else {
            io_error("open", path, e)
        }
    })
}
pub fn read(root: &Path, target: &str, max: usize) -> Result<String, RpcError> {
    if !contains(root, target) {
        return Err(outside());
    }
    let path = pinned_parent(&resolved(root, target), root)?;
    let mut file = open(&path, false)?;
    let metadata = verify(&file, &path, root)?;
    if !metadata.is_file() {
        return Err(refused("Not a file"));
    }
    if metadata.len() > max as u64 {
        return Err(refused("File too large"));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(max.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error("read", &path, e))?;
    if bytes.len() > max {
        return Err(refused("File too large"));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
pub fn write(root: &Path, target: &str, text: &str, max: usize) -> Result<usize, RpcError> {
    let text = node_text(text);
    if text.len() > max {
        return Err(refused("File too large"));
    }
    if !contains(root, target) {
        return Err(outside());
    }
    let target = resolved(root, target);
    let real_root = fs::canonicalize(root).map_err(|_| outside())?;
    let dir = target.parent().ok_or_else(outside)?;
    let mut existing = dir.to_path_buf();
    while !existing.exists() {
        if fs::symlink_metadata(&existing).is_ok() {
            return Err(outside());
        }
        if !existing.pop() {
            return Err(outside());
        }
    }
    if existing != dir
        && fs::symlink_metadata(&existing)
            .map_err(|_| outside())?
            .file_type()
            .is_symlink()
    {
        return Err(outside());
    }
    if !beneath(
        &real_root,
        &fs::canonicalize(&existing).map_err(|_| outside())?,
    ) {
        return Err(outside());
    }
    if existing != dir {
        fs::create_dir_all(dir).map_err(|e| io_error("mkdir", dir, e))?;
    }
    let path = pinned_parent(&target, root)?;
    let mut file = open(&path, true)?;
    if !verify(&file, &path, root)?.is_file() {
        return Err(refused("Not a file"));
    }
    file.set_len(0)
        .map_err(|e| io_error("ftruncate", &path, e))?;
    file.write_all(text.as_bytes())
        .map_err(|e| io_error("write", &path, e))?;
    Ok(text.len())
}
fn io_error(operation: &str, path: &Path, error: std::io::Error) -> RpcError {
    let (code, message) = match error.raw_os_error() {
        Some(libc::ENOENT) => ("ENOENT", "no such file or directory"),
        Some(libc::EACCES) => ("EACCES", "permission denied"),
        Some(libc::ENOTDIR) => ("ENOTDIR", "not a directory"),
        Some(libc::EISDIR) => ("EISDIR", "illegal operation on a directory"),
        Some(libc::ENOSPC) => ("ENOSPC", "no space left on device"),
        Some(libc::EEXIST) => ("EEXIST", "file already exists"),
        _ => {
            return RpcError {
                code: -32603,
                message: error.to_string(),
                data: None,
            }
        }
    };
    RpcError {
        code: -32603,
        message: format!("{code}: {message}, {operation} '{}'", path.display()),
        data: None,
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use js_json::JValue;
    use std::os::unix::fs::symlink;

    #[test]
    fn rooted_access_refuses_external_files_and_links_without_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, "preserved").unwrap();
        assert!(!contains(&root, outside.to_str().unwrap()));
        assert!(write(&root, "../outside", "bad", MAX_FILE_BYTES).is_err());
        symlink(&outside, root.join("alias")).unwrap();
        let fixture = wtf16::parse(include_str!("../tests/fixtures/files-node.json")).unwrap();
        let alias = pinned_parent(&root.join("alias"), &root).unwrap_err();
        let JValue::Arr(refusals) = fixture.get("refusals") else {
            unreachable!()
        };
        assert_eq!(alias.message, refusals[0].get("message").as_str().unwrap());
        assert_eq!(alias.code, -32602);
        fs::hard_link(&outside, root.join("hard")).unwrap();
        let hard = write(&root, "hard", "bad", MAX_FILE_BYTES).unwrap_err();
        assert_eq!(hard.code, -32602);
        assert_eq!(hard.message, refusals[1].get("message").as_str().unwrap());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "preserved");
    }

    #[test]
    fn creates_nested_files_and_enforces_byte_limits() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(write(dir.path(), "nested/file", "hello", 5).unwrap(), 5);
        assert_eq!(read(dir.path(), "nested/file", 5).unwrap(), "hello");
        assert!(write(dir.path(), "nested/file", "ééé", 5).is_err());
        assert!(read(dir.path(), "nested/file", 4).is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("nested/file")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn dangling_and_directory_symlinks_do_not_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        symlink(dir.path().join("missing"), root.join("dangling")).unwrap();
        symlink(dir.path(), root.join("parent")).unwrap();
        assert!(!contains(&root, "dangling"));
        assert!(!contains(&root, "parent/new"));
        assert!(write(&root, "parent/new", "bad", MAX_FILE_BYTES).is_err());
        assert!(!dir.path().join("new").exists());
    }
}
