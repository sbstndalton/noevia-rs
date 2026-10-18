use rustix::fs::{fstat, openat, openat2, FileType, Mode, OFlags, RawDir, ResolveFlags, CWD};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

pub const MAX_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 512;
pub const MAX_DEPTH: usize = 32;
const MAX_MS: u128 = 2_000;
const OWNER: &str = "legacy-owner.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimError {
    InvalidUser,
    Unsafe,
    Limit,
    Io(String),
}
impl From<std::io::Error> for ClaimError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}
fn rustix_error(e: rustix::io::Errno) -> ClaimError {
    if e == rustix::io::Errno::LOOP
        || e == rustix::io::Errno::XDEV
        || e == rustix::io::Errno::NOSYS
        || e == rustix::io::Errno::NOTSUP
        || e == rustix::io::Errno::INVAL
    {
        ClaimError::Unsafe
    } else {
        ClaimError::Io(e.to_string())
    }
}
fn check_time(start: Instant) -> Result<(), ClaimError> {
    if start.elapsed() > std::time::Duration::from_millis(MAX_MS as u64) {
        Err(ClaimError::Limit)
    } else {
        Ok(())
    }
}
fn uuid(id: &str) -> bool {
    id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}
fn lstat(path: &Path) -> Result<Option<fs::Metadata>, ClaimError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn dir(path: &Path) -> Result<(), ClaimError> {
    match lstat(path)? {
        Some(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(()),
        Some(_) => Err(ClaimError::Unsafe),
        None => {
            fs::create_dir(path)?;
            set_mode(path, 0o700)?;
            Ok(())
        }
    }
}
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), ClaimError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), ClaimError> {
    Ok(())
}

fn open_root(path: &Path) -> Result<OwnedFd, ClaimError> {
    openat(
        CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(rustix_error)
}
fn source_fd(root: &OwnedFd, rel: &Path, flags: OFlags) -> Result<OwnedFd, ClaimError> {
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(ClaimError::Unsafe);
    }
    openat2(
        root.as_fd(),
        rel,
        flags | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(rustix_error)
}
fn source_stat(root: &OwnedFd, rel: &Path) -> Result<Option<rustix::fs::Stat>, ClaimError> {
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(ClaimError::Unsafe);
    }
    let fd = match openat2(
        root.as_fd(),
        rel,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    ) {
        Ok(fd) => fd,
        Err(e) if e == rustix::io::Errno::NOENT => return Ok(None),
        Err(e) => return Err(rustix_error(e)),
    };
    fstat(&fd).map(Some).map_err(rustix_error)
}
fn directory_names(
    root: &OwnedFd,
    rel: &Path,
    start: Instant,
    cap: usize,
) -> Result<Vec<std::ffi::OsString>, ClaimError> {
    let dirfd = if rel.as_os_str().is_empty() || rel == Path::new(".") {
        openat(
            root.as_fd(),
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(rustix_error)?
    } else {
        source_fd(root, rel, OFlags::RDONLY | OFlags::DIRECTORY)?
    };
    let mut buffer = [MaybeUninit::<u8>::uninit(); 8192];
    let mut entries = RawDir::new(dirfd, &mut buffer);
    let mut names = Vec::new();
    while let Some(entry) = entries.next() {
        check_time(start)?;
        let entry = entry.map_err(rustix_error)?;
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if names.len() >= cap {
            return Err(ClaimError::Limit);
        }
        names.push(std::ffi::OsString::from_vec(bytes.to_vec()));
    }
    names.sort();
    Ok(names)
}
fn has_legacy(root: &OwnedFd, start: Instant) -> Result<bool, ClaimError> {
    for name in ["projects.json", "providers.json"] {
        if source_stat(root, Path::new(name))?.is_some() {
            return Ok(true);
        }
    }
    for name in directory_names(root, Path::new("."), start, MAX_ENTRIES)? {
        check_time(start)?;
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("history-") && name.ends_with(".json") {
            return Ok(true);
        }
    }
    Ok(false)
}
fn names(root: &OwnedFd, start: Instant) -> Result<Vec<String>, ClaimError> {
    let mut out = Vec::new();
    for name in directory_names(root, Path::new("."), start, MAX_ENTRIES)? {
        check_time(start)?;
        let Some(name) = name.to_str() else { continue };
        if [
            "projects.json",
            "providers.json",
            "free-chats.json",
            "auto-roles.json",
        ]
        .contains(&name)
            || (name.starts_with("history-") && name.ends_with(".json"))
            || name == "rag"
        {
            out.push(name.to_owned());
        }
    }
    out.sort();
    Ok(out)
}
fn owner(root: &OwnedFd) -> Result<Option<String>, ClaimError> {
    let path = Path::new(OWNER);
    let Some(meta) = source_stat(root, path)? else {
        return Ok(None);
    };
    if FileType::from_raw_mode(meta.st_mode) != FileType::RegularFile || meta.st_size > 4096 {
        return Ok(Some(String::new()));
    }
    let fd = source_fd(root, path, OFlags::RDONLY)?;
    let file = File::from(fd);
    let mut bytes = Vec::with_capacity(4097);
    file.take(4097).read_to_end(&mut bytes)?;
    if bytes.len() > 4096 {
        return Ok(Some(String::new()));
    }
    let text = String::from_utf8_lossy(&bytes);
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Ok(Some(String::new())),
    };
    Ok(Some(
        parsed
            .get("userId")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
    ))
}
fn previous_owner(root: &OwnedFd, start: Instant) -> Result<Option<String>, ClaimError> {
    let Some(meta) = source_stat(root, Path::new("users"))? else {
        return Ok(None);
    };
    if FileType::from_raw_mode(meta.st_mode) != FileType::Directory {
        return Err(ClaimError::Unsafe);
    }
    let mut ids = Vec::new();
    for entry in directory_names(root, Path::new("users"), start, MAX_ENTRIES)? {
        check_time(start)?;
        let Some(id) = entry.to_str() else { continue };
        if uuid(id) {
            ids.push(id.to_owned());
        }
    }
    // Node sorts readdirSync results before examining migration records.
    ids.sort();
    for id in ids {
        check_time(start)?;
        let d = Path::new("users").join(&id);
        let migration = d.join("migration.json");
        if let (Some(ds), Some(ms)) = (source_stat(root, &d)?, source_stat(root, &migration)?) {
            if FileType::from_raw_mode(ds.st_mode) == FileType::Directory
                && FileType::from_raw_mode(ms.st_mode) == FileType::RegularFile
            {
                return Ok(Some(id));
            }
        }
    }
    Ok(None)
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Item {
    rel: PathBuf,
    dir: bool,
    size: u64,
    digest: Option<[u8; 32]>,
}
fn inspect(root: &OwnedFd, candidates: &[String], start: Instant) -> Result<Vec<Item>, ClaimError> {
    fn visit(
        root: &OwnedFd,
        rel: &Path,
        depth: usize,
        start: Instant,
        out: &mut Vec<Item>,
        bytes: &mut u64,
    ) -> Result<(), ClaimError> {
        check_time(start)?;
        if depth > MAX_DEPTH {
            return Err(ClaimError::Limit);
        }
        let meta = source_stat(root, rel)?.ok_or(ClaimError::Unsafe)?;
        let kind = FileType::from_raw_mode(meta.st_mode);
        if out.len() >= MAX_ENTRIES {
            return Err(ClaimError::Limit);
        }
        if kind == FileType::RegularFile {
            let size = u64::try_from(meta.st_size).map_err(|_| ClaimError::Unsafe)?;
            *bytes = bytes.checked_add(size).ok_or(ClaimError::Limit)?;
            if *bytes > MAX_BYTES {
                return Err(ClaimError::Limit);
            }
            let fd = source_fd(root, rel, OFlags::RDONLY)?;
            let opened = fstat(&fd).map_err(rustix_error)?;
            if FileType::from_raw_mode(opened.st_mode) != FileType::RegularFile
                || opened.st_size != meta.st_size
            {
                return Err(ClaimError::Unsafe);
            }
            let mut input = File::from(fd);
            let mut hash = Sha256::new();
            let mut read = 0u64;
            let mut buffer = [0u8; 32 * 1024];
            loop {
                check_time(start)?;
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                read = read.checked_add(count as u64).ok_or(ClaimError::Limit)?;
                if read > size {
                    return Err(ClaimError::Unsafe);
                }
                hash.update(&buffer[..count]);
            }
            if read != size {
                return Err(ClaimError::Unsafe);
            }
            out.push(Item {
                rel: rel.to_path_buf(),
                dir: false,
                size,
                digest: Some(hash.finalize().into()),
            });
        } else if kind == FileType::Directory {
            out.push(Item {
                rel: rel.to_path_buf(),
                dir: true,
                size: 0,
                digest: None,
            });
            let cap = MAX_ENTRIES.saturating_sub(out.len());
            for child in directory_names(root, rel, start, cap)? {
                visit(root, &rel.join(child), depth + 1, start, out, bytes)?;
            }
        } else {
            return Err(ClaimError::Unsafe);
        }
        Ok(())
    }
    let mut out = Vec::new();
    let mut bytes = 0;
    for candidate in candidates {
        visit(root, Path::new(candidate), 0, start, &mut out, &mut bytes)?;
    }
    check_time(start)?;
    Ok(out)
}
fn random_suffix() -> Result<String, ClaimError> {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; 16];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| ClaimError::Io(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn create_stage(parent: &Path, suffix: &str) -> Result<PathBuf, ClaimError> {
    let path = parent.join(format!(".legacy-stage-{suffix}"));
    fs::create_dir(&path)?;
    set_mode(&path, 0o700)?;
    Ok(path)
}
fn copy_tree(
    root: &OwnedFd,
    rel: &Path,
    dest: &Path,
    start: Instant,
    bytes: &mut u64,
    entries: &mut usize,
    depth: usize,
    expected: &BTreeMap<PathBuf, &Item>,
) -> Result<(), ClaimError> {
    check_time(start)?;
    if depth > MAX_DEPTH {
        return Err(ClaimError::Limit);
    }
    *entries = entries.checked_add(1).ok_or(ClaimError::Limit)?;
    if *entries > MAX_ENTRIES {
        return Err(ClaimError::Limit);
    }
    let item = expected.get(rel).ok_or(ClaimError::Unsafe)?;
    let meta = source_stat(root, rel)?.ok_or(ClaimError::Unsafe)?;
    let kind = FileType::from_raw_mode(meta.st_mode);
    if kind == FileType::Directory && item.dir {
        fs::create_dir(dest)?;
        set_mode(dest, 0o700)?;
        let child_count = expected
            .keys()
            .filter(|path| path.parent() == Some(rel))
            .count();
        let children = directory_names(root, rel, start, child_count)?;
        for child in children {
            let child_rel = rel.join(&child);
            if !expected.contains_key(&child_rel) {
                return Err(ClaimError::Unsafe);
            }
            copy_tree(
                root,
                &child_rel,
                &dest.join(child),
                start,
                bytes,
                entries,
                depth + 1,
                expected,
            )?;
        }
    } else if kind == FileType::RegularFile && !item.dir {
        if u64::try_from(meta.st_size).map_err(|_| ClaimError::Unsafe)? != item.size {
            return Err(ClaimError::Unsafe);
        }
        let fd = source_fd(root, rel, OFlags::RDONLY)?;
        let opened = fstat(&fd).map_err(rustix_error)?;
        if FileType::from_raw_mode(opened.st_mode) != FileType::RegularFile
            || opened.st_size != meta.st_size
        {
            return Err(ClaimError::Unsafe);
        }
        let mut input = File::from(fd);
        let mut output = OpenOptions::new().write(true).create_new(true).open(dest)?;
        let mut buffer = [0u8; 32 * 1024];
        let mut copied = 0u64;
        let mut hash = Sha256::new();
        loop {
            check_time(start)?;
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            copied = copied.checked_add(count as u64).ok_or(ClaimError::Limit)?;
            *bytes = bytes.checked_add(count as u64).ok_or(ClaimError::Limit)?;
            if *bytes > MAX_BYTES || copied > item.size {
                return Err(ClaimError::Limit);
            }
            hash.update(&buffer[..count]);
            output.write_all(&buffer[..count])?;
        }
        if copied != item.size || Some(<[u8; 32]>::from(hash.finalize())) != item.digest {
            return Err(ClaimError::Unsafe);
        }
        output.sync_all()?;
        set_mode(dest, 0o600)?;
    } else {
        return Err(ClaimError::Unsafe);
    }
    Ok(())
}
fn stage_names(
    path: &Path,
    start: Instant,
    cap: usize,
) -> Result<Vec<std::ffi::OsString>, ClaimError> {
    let mut names = Vec::new();
    for entry in fs::read_dir(path)? {
        check_time(start)?;
        if names.len() >= cap {
            return Err(ClaimError::Limit);
        }
        names.push(entry?.file_name());
    }
    names.sort();
    Ok(names)
}
fn validate(stage: &Path, manifest: &[Item], start: Instant) -> Result<(), ClaimError> {
    let expected: BTreeMap<PathBuf, &Item> = manifest.iter().map(|i| (i.rel.clone(), i)).collect();
    let mut seen = BTreeSet::new();
    fn visit(
        stage: &Path,
        rel: &Path,
        depth: usize,
        start: Instant,
        expected: &BTreeMap<PathBuf, &Item>,
        seen: &mut BTreeSet<PathBuf>,
    ) -> Result<(), ClaimError> {
        check_time(start)?;
        if depth > MAX_DEPTH || seen.len() >= MAX_ENTRIES {
            return Err(ClaimError::Limit);
        }
        let path = stage.join(rel);
        let meta = fs::symlink_metadata(&path)?;
        let item = expected.get(rel).ok_or(ClaimError::Unsafe)?;
        if meta.file_type().is_symlink()
            || meta.is_dir() != item.dir
            || meta.is_file() == item.dir
            || (!item.dir && meta.len() != item.size)
        {
            return Err(ClaimError::Unsafe);
        }
        seen.insert(rel.to_path_buf());
        if item.dir {
            set_mode(&path, 0o700)?;
            let child_count = expected
                .keys()
                .filter(|child| child.parent() == Some(rel))
                .count();
            for child in stage_names(&path, start, child_count)? {
                let child_rel = rel.join(child);
                visit(stage, &child_rel, depth + 1, start, expected, seen)?;
            }
        } else {
            set_mode(&path, 0o600)?;
        }
        Ok(())
    }
    for item in manifest.iter().filter(|i| i.rel.components().count() == 1) {
        visit(stage, &item.rel, 0, start, &expected, &mut seen)?;
    }
    if seen.len() != expected.len() {
        return Err(ClaimError::Unsafe);
    }
    Ok(())
}
fn cleanup_stage(path: &Path) {
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.is_dir() && !meta.file_type().is_symlink() {
            let _ = fs::remove_dir_all(path);
        }
    }
}
fn write_owner(root: &OwnedFd, id: &str) -> Result<(), ClaimError> {
    let fd = openat(
        root.as_fd(),
        OWNER,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(rustix_error)?;
    let mut file = File::from(fd);
    file.write_all(format!(r#"{{"userId":"{id}"}}"#).as_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// Claim legacy files for this account. Caller holds the auth DB IMMEDIATE transaction and the
/// dedicated root lock; callers must revalidate an administrator inside that transaction.
pub fn claim_root(root: &Path, user_id: &str) -> Result<bool, ClaimError> {
    if !uuid(user_id) {
        return Err(ClaimError::InvalidUser);
    }
    let start = Instant::now();
    let root_fd = open_root(root)?;
    if !has_legacy(&root_fd, start)? {
        return Ok(false);
    }
    check_time(start)?;
    let stored = owner(&root_fd)?;
    if stored.as_deref() == Some("") {
        return Ok(false);
    }
    let owner_id = match stored.as_ref() {
        Some(id) => id.clone(),
        None => previous_owner(&root_fd, start)?.unwrap_or_else(|| user_id.to_string()),
    };
    let target_rel = Path::new("users").join(user_id);
    let target = root.join(&target_rel);
    if owner_id != user_id || source_stat(&root_fd, &target_rel)?.is_some() {
        if source_stat(&root_fd, Path::new(OWNER))?.is_none() {
            let _ = write_owner(&root_fd, &owner_id);
        }
        return Ok(false);
    }
    let candidates = names(&root_fd, start)?;
    check_time(start)?;
    let manifest = inspect(&root_fd, &candidates, start)?;
    // Re-scan the root before any output is created. A new candidate added after
    // preflight is a refusal instead of a partial migration.
    if names(&root_fd, start)? != candidates {
        return Err(ClaimError::Unsafe);
    }
    let expected: BTreeMap<PathBuf, &Item> = manifest
        .iter()
        .map(|item| (item.rel.clone(), item))
        .collect();
    let users = root.join("users");
    let backups = root.join("migration-backups");
    dir(&users)?;
    dir(&backups)?;
    let suffix = format!(
        "{}-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        std::process::id(),
        random_suffix()?
    );
    let backup = backups.join(format!("legacy-{suffix}"));
    let backup_stage = create_stage(&backups, &suffix)?;
    let target_stage = match create_stage(&users, &suffix) {
        Ok(p) => p,
        Err(e) => {
            cleanup_stage(&backup_stage);
            return Err(e);
        }
    };
    let mut backup_published = false;
    let mut target_published = false;
    let result = (|| {
        let mut backup_bytes = 0u64;
        let mut target_bytes = 0u64;
        let mut backup_entries = 0usize;
        let mut target_entries = 0usize;
        for name in &candidates {
            check_time(start)?;
            copy_tree(
                &root_fd,
                Path::new(name),
                &backup_stage.join(name),
                start,
                &mut backup_bytes,
                &mut backup_entries,
                0,
                &expected,
            )?;
            copy_tree(
                &root_fd,
                Path::new(name),
                &target_stage.join(name),
                start,
                &mut target_bytes,
                &mut target_entries,
                0,
                &expected,
            )?;
        }
        validate(&backup_stage, &manifest, start)?;
        validate(&target_stage, &manifest, start)?;
        let marker = serde_json::json!({ "version": 1, "source": "legacy-root", "backup": backup.to_string_lossy(), "migratedAt": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() });
        let marker_path = target_stage.join("migration.json");
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker_path)?;
        f.write_all(
            serde_json::to_string_pretty(&marker)
                .map_err(|e| ClaimError::Io(e.to_string()))?
                .as_bytes(),
        )?;
        f.sync_all()?;
        set_mode(&target_stage.join("migration.json"), 0o600)?;
        check_time(start)?;
        fs::rename(&backup_stage, &backup)?;
        backup_published = true;
        fs::rename(&target_stage, &target)?;
        target_published = true;
        if stored.is_none() {
            write_owner(&root_fd, &owner_id)?;
        }
        check_time(start)?;
        Ok(true)
    })();
    if !backup_published {
        cleanup_stage(&backup_stage);
    }
    if !target_published {
        cleanup_stage(&target_stage);
    }
    result
}

#[cfg(test)]
mod security_tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::*;
    use tempfile::tempdir;

    fn expected(manifest: &[Item]) -> BTreeMap<PathBuf, &Item> {
        manifest
            .iter()
            .map(|item| (item.rel.clone(), item))
            .collect()
    }

    #[test]
    fn growth_after_preflight_is_bounded_and_refused() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("rag")).unwrap();
        fs::write(root.path().join("rag/first"), b"first").unwrap();
        let fd = open_root(root.path()).unwrap();
        let start = Instant::now();
        let manifest = inspect(&fd, &["rag".to_owned()], start).unwrap();
        let expected = expected(&manifest);

        fs::write(
            root.path().join("rag/added-after-preflight"),
            vec![0u8; 128],
        )
        .unwrap();
        let stage = tempdir().unwrap();
        let mut bytes = 0;
        let mut entries = 0;
        let result = copy_tree(
            &fd,
            Path::new("rag"),
            &stage.path().join("rag"),
            start,
            &mut bytes,
            &mut entries,
            0,
            &expected,
        );
        assert!(matches!(
            result,
            Err(ClaimError::Limit | ClaimError::Unsafe)
        ));
        assert!(!stage.path().join("rag/added-after-preflight").exists());
        assert_eq!(
            fs::read(root.path().join("rag/added-after-preflight")).unwrap(),
            vec![0u8; 128]
        );
    }

    #[cfg(unix)]
    #[test]
    fn ancestor_symlink_swap_after_preflight_cannot_escape_root() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::create_dir(root.path().join("rag")).unwrap();
        fs::write(root.path().join("rag/original"), b"inside").unwrap();
        fs::write(outside.path().join("original"), b"outside-secret").unwrap();
        let fd = open_root(root.path()).unwrap();
        let start = Instant::now();
        let manifest = inspect(&fd, &["rag".to_owned()], start).unwrap();
        let expected = expected(&manifest);

        fs::rename(root.path().join("rag"), root.path().join("rag-saved")).unwrap();
        symlink(outside.path(), root.path().join("rag")).unwrap();
        let stage = tempdir().unwrap();
        let mut bytes = 0;
        let mut entries = 0;
        let result = copy_tree(
            &fd,
            Path::new("rag"),
            &stage.path().join("rag"),
            start,
            &mut bytes,
            &mut entries,
            0,
            &expected,
        );
        assert!(matches!(result, Err(ClaimError::Unsafe)));
        assert!(!stage.path().join("rag/original").exists());
        assert_eq!(
            fs::read(outside.path().join("original")).unwrap(),
            b"outside-secret"
        );
    }
}
