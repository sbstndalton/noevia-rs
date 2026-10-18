use rustix::fs::{
    fchmod, fstat, mkdirat, openat, openat2, renameat_with, statat, unlinkat, AtFlags, FileType,
    Mode, OFlags, RawDir, RenameFlags, ResolveFlags, CWD,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
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
fn open_root(path: &Path) -> Result<OwnedFd, ClaimError> {
    // UI_DATA_DIR is configuration, but still refuse symlinks in any component so the
    // descriptor tree cannot be redirected between startup validation and a claim.
    if !path.is_absolute()
        || path.components().any(|c| {
            !matches!(
                c,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
    {
        return Err(ClaimError::Unsafe);
    }
    openat2(
        CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )
    .map_err(rustix_error)
}
fn open_dir_at(parent: &OwnedFd, name: &std::ffi::OsStr) -> Result<OwnedFd, ClaimError> {
    openat(
        parent.as_fd(),
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(rustix_error)
}
fn ensure_dir_at(parent: &OwnedFd, name: &str) -> Result<OwnedFd, ClaimError> {
    match mkdirat(parent.as_fd(), name, Mode::from_raw_mode(0o700)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(rustix_error(e)),
    }
    open_dir_at(parent, std::ffi::OsStr::new(name))
}
fn create_stage_at(parent: &OwnedFd, name: &str) -> Result<OwnedFd, ClaimError> {
    mkdirat(parent.as_fd(), name, Mode::from_raw_mode(0o700)).map_err(rustix_error)?;
    let fd = open_dir_at(parent, std::ffi::OsStr::new(name))?;
    fchmod(&fd, Mode::from_raw_mode(0o700)).map_err(rustix_error)?;
    Ok(fd)
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
fn source_regular_file(
    root: &OwnedFd,
    rel: &Path,
    expected_size: i64,
) -> Result<(OwnedFd, rustix::fs::Stat), ClaimError> {
    // NONBLOCK ensures an entry swapped to a FIFO after source_stat cannot hold the
    // auth writer transaction indefinitely. Verify the opened inode before reading it.
    let fd = source_fd(
        root,
        rel,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY,
    )?;
    let opened = fstat(&fd).map_err(rustix_error)?;
    if FileType::from_raw_mode(opened.st_mode) != FileType::RegularFile
        || opened.st_size < 0
        || opened.st_size != expected_size
    {
        return Err(ClaimError::Unsafe);
    }
    Ok((fd, opened))
}
fn directory_names_fd(
    dirfd: &OwnedFd,
    start: Instant,
    cap: usize,
) -> Result<Vec<std::ffi::OsString>, ClaimError> {
    let scanfd = openat(
        dirfd.as_fd(),
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(rustix_error)?;
    let mut buffer = [MaybeUninit::<u8>::uninit(); 8192];
    let mut entries = RawDir::new(scanfd, &mut buffer);
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
    directory_names_fd(&dirfd, start, cap)
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
    let (fd, _) = source_regular_file(root, path, meta.st_size)?;
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
            let (fd, _) = source_regular_file(root, rel, meta.st_size)?;
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
                hash.update(buffer.get(..count).ok_or(ClaimError::Unsafe)?);
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
struct CopyBudget {
    start: Instant,
    bytes: u64,
    entries: usize,
}
fn copy_tree(
    root: &OwnedFd,
    rel: &Path,
    dest_parent: &OwnedFd,
    dest_name: &std::ffi::OsStr,
    budget: &mut CopyBudget,
    depth: usize,
    expected: &BTreeMap<PathBuf, &Item>,
) -> Result<(), ClaimError> {
    check_time(budget.start)?;
    if depth > MAX_DEPTH {
        return Err(ClaimError::Limit);
    }
    budget.entries = budget.entries.checked_add(1).ok_or(ClaimError::Limit)?;
    if budget.entries > MAX_ENTRIES {
        return Err(ClaimError::Limit);
    }
    let item = expected.get(rel).ok_or(ClaimError::Unsafe)?;
    let meta = source_stat(root, rel)?.ok_or(ClaimError::Unsafe)?;
    let kind = FileType::from_raw_mode(meta.st_mode);
    if kind == FileType::Directory && item.dir {
        mkdirat(dest_parent.as_fd(), dest_name, Mode::from_raw_mode(0o700))
            .map_err(rustix_error)?;
        let child_fd = openat(
            dest_parent.as_fd(),
            dest_name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(rustix_error)?;
        fchmod(&child_fd, Mode::from_raw_mode(0o700)).map_err(rustix_error)?;
        let child_count = expected
            .keys()
            .filter(|path| path.parent() == Some(rel))
            .count();
        let children = directory_names(root, rel, budget.start, child_count)?;
        for child in children {
            let child_rel = rel.join(&child);
            if !expected.contains_key(&child_rel) {
                return Err(ClaimError::Unsafe);
            }
            copy_tree(
                root,
                &child_rel,
                &child_fd,
                &child,
                budget,
                depth + 1,
                expected,
            )?;
        }
    } else if kind == FileType::RegularFile && !item.dir {
        if u64::try_from(meta.st_size).map_err(|_| ClaimError::Unsafe)? != item.size {
            return Err(ClaimError::Unsafe);
        }
        let (fd, _) = source_regular_file(root, rel, meta.st_size)?;
        let mut input = File::from(fd);
        let output_fd = openat(
            dest_parent.as_fd(),
            dest_name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(rustix_error)?;
        let output_stat = fstat(&output_fd).map_err(rustix_error)?;
        if FileType::from_raw_mode(output_stat.st_mode) != FileType::RegularFile {
            return Err(ClaimError::Unsafe);
        }
        let mut output = File::from(output_fd);
        let mut buffer = [0u8; 32 * 1024];
        let mut copied = 0u64;
        let mut hash = Sha256::new();
        loop {
            check_time(budget.start)?;
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            copied = copied.checked_add(count as u64).ok_or(ClaimError::Limit)?;
            budget.bytes = budget
                .bytes
                .checked_add(count as u64)
                .ok_or(ClaimError::Limit)?;
            if budget.bytes > MAX_BYTES || copied > item.size {
                return Err(ClaimError::Limit);
            }
            let chunk = buffer.get(..count).ok_or(ClaimError::Unsafe)?;
            hash.update(chunk);
            output.write_all(chunk)?;
        }
        if copied != item.size || Some(<[u8; 32]>::from(hash.finalize())) != item.digest {
            return Err(ClaimError::Unsafe);
        }
        output.sync_all()?;
        fchmod(output.as_fd(), Mode::from_raw_mode(0o600)).map_err(rustix_error)?;
    } else {
        return Err(ClaimError::Unsafe);
    }
    Ok(())
}
fn validate(stage: &OwnedFd, manifest: &[Item], start: Instant) -> Result<(), ClaimError> {
    let expected: BTreeMap<PathBuf, &Item> = manifest.iter().map(|i| (i.rel.clone(), i)).collect();
    let mut seen = BTreeSet::new();
    fn visit(
        stage: &OwnedFd,
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
        let meta = source_stat(stage, rel)?.ok_or(ClaimError::Unsafe)?;
        let item = expected.get(rel).ok_or(ClaimError::Unsafe)?;
        if FileType::from_raw_mode(meta.st_mode) == FileType::Directory {
            if !item.dir {
                return Err(ClaimError::Unsafe);
            }
            let fd = source_fd(stage, rel, OFlags::RDONLY | OFlags::DIRECTORY)?;
            fchmod(&fd, Mode::from_raw_mode(0o700)).map_err(rustix_error)?;
            seen.insert(rel.to_path_buf());
            let child_count = expected
                .keys()
                .filter(|child| child.parent() == Some(rel))
                .count();
            for child in directory_names(stage, rel, start, child_count)? {
                let child_rel = rel.join(child);
                visit(stage, &child_rel, depth + 1, start, expected, seen)?;
            }
        } else if FileType::from_raw_mode(meta.st_mode) == FileType::RegularFile && !item.dir {
            if meta.st_size < 0 || meta.st_size as u64 != item.size {
                return Err(ClaimError::Unsafe);
            }
            let fd = source_fd(stage, rel, OFlags::RDONLY)?;
            fchmod(&fd, Mode::from_raw_mode(0o600)).map_err(rustix_error)?;
            seen.insert(rel.to_path_buf());
        } else {
            return Err(ClaimError::Unsafe);
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
fn cleanup_stage_at(parent: &OwnedFd, name: &str) {
    fn clear(dir: &OwnedFd, depth: usize, total: &mut usize) -> Result<(), ClaimError> {
        if depth > MAX_DEPTH {
            return Err(ClaimError::Limit);
        }
        let remaining = MAX_ENTRIES.saturating_sub(*total);
        let names = directory_names_fd(dir, Instant::now(), remaining)?;
        for name in names {
            *total = total.checked_add(1).ok_or(ClaimError::Limit)?;
            if *total > MAX_ENTRIES {
                return Err(ClaimError::Limit);
            }
            match statat(dir.as_fd(), &name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) if FileType::from_raw_mode(st.st_mode) == FileType::Directory => {
                    let child = open_dir_at(dir, &name)?;
                    clear(&child, depth + 1, total)?;
                    unlinkat(dir.as_fd(), &name, AtFlags::REMOVEDIR).map_err(rustix_error)?;
                }
                Ok(_) => unlinkat(dir.as_fd(), &name, AtFlags::empty()).map_err(rustix_error)?,
                Err(e) => return Err(rustix_error(e)),
            }
        }
        Ok(())
    }
    if let Ok(stage) = open_dir_at(parent, std::ffi::OsStr::new(name)) {
        let _ = clear(&stage, 0, &mut 0);
        let _ = unlinkat(parent.as_fd(), name, AtFlags::REMOVEDIR);
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
    let users_fd = ensure_dir_at(&root_fd, "users")?;
    let backups_fd = ensure_dir_at(&root_fd, "migration-backups")?;
    let suffix = format!(
        "{}-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        std::process::id(),
        random_suffix()?
    );
    let backup_name = format!("legacy-{suffix}");
    let backup_path = root.join("migration-backups").join(&backup_name);
    let backup_stage_name = format!(".legacy-stage-{suffix}");
    let target_stage_name = format!(".legacy-stage-{suffix}");
    let backup_stage = create_stage_at(&backups_fd, &backup_stage_name)?;
    let target_stage = match create_stage_at(&users_fd, &target_stage_name) {
        Ok(fd) => fd,
        Err(e) => {
            cleanup_stage_at(&backups_fd, &backup_stage_name);
            return Err(e);
        }
    };
    let mut backup_published = false;
    let mut target_published = false;
    let result = (|| {
        let mut backup_budget = CopyBudget {
            start,
            bytes: 0,
            entries: 0,
        };
        let mut target_budget = CopyBudget {
            start,
            bytes: 0,
            entries: 0,
        };
        for name in &candidates {
            check_time(start)?;
            copy_tree(
                &root_fd,
                Path::new(name),
                &backup_stage,
                std::ffi::OsStr::new(name),
                &mut backup_budget,
                0,
                &expected,
            )?;
            copy_tree(
                &root_fd,
                Path::new(name),
                &target_stage,
                std::ffi::OsStr::new(name),
                &mut target_budget,
                0,
                &expected,
            )?;
        }
        validate(&backup_stage, &manifest, start)?;
        validate(&target_stage, &manifest, start)?;
        let marker = serde_json::json!({ "version": 1, "source": "legacy-root", "backup": backup_path.to_string_lossy(), "migratedAt": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() });
        let fd = openat(
            target_stage.as_fd(),
            "migration.json",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(rustix_error)?;
        let mut file = File::from(fd);
        file.write_all(
            serde_json::to_string_pretty(&marker)
                .map_err(|e| ClaimError::Io(e.to_string()))?
                .as_bytes(),
        )?;
        file.sync_all()?;
        check_time(start)?;
        renameat_with(
            &backups_fd,
            &backup_stage_name,
            &backups_fd,
            &backup_name,
            RenameFlags::NOREPLACE,
        )
        .map_err(rustix_error)?;
        backup_published = true;
        renameat_with(
            &users_fd,
            &target_stage_name,
            &users_fd,
            user_id,
            RenameFlags::NOREPLACE,
        )
        .map_err(rustix_error)?;
        target_published = true;
        if stored.is_none() {
            write_owner(&root_fd, &owner_id)?;
        }
        check_time(start)?;
        Ok(true)
    })();
    if !backup_published {
        cleanup_stage_at(&backups_fd, &backup_stage_name);
    }
    if !target_published {
        cleanup_stage_at(&users_fd, &target_stage_name);
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
    use std::fs;
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
        let stage_fd = open_root(stage.path()).unwrap();
        let mut budget = CopyBudget {
            start,
            bytes: 0,
            entries: 0,
        };
        let result = copy_tree(
            &fd,
            Path::new("rag"),
            &stage_fd,
            std::ffi::OsStr::new("rag"),
            &mut budget,
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

    #[cfg(target_os = "linux")]
    #[test]
    fn fifo_replacement_does_not_block_regular_file_open() {
        const CHILD_ROOT: &str = "NOEVIA_FIFO_REPLACEMENT_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let root_fd = open_root(&root).unwrap();
            for name in [OWNER, "projects.json"] {
                let path = Path::new(name);
                fs::write(root.join(path), b"synthetic regular file").unwrap();
                let before = source_stat(&root_fd, path).unwrap().unwrap();
                assert_eq!(
                    FileType::from_raw_mode(before.st_mode),
                    FileType::RegularFile
                );
                fs::remove_file(root.join(path)).unwrap();
                rustix::fs::mkfifoat(root_fd.as_fd(), name, Mode::from_raw_mode(0o600)).unwrap();
                assert!(matches!(
                    source_regular_file(&root_fd, path, before.st_size),
                    Err(ClaimError::Unsafe)
                ));
            }
            return;
        }

        let root = tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("legacy_claim::security_tests::fifo_replacement_does_not_block_regular_file_open")
            .arg("--nocapture")
            .env(CHILD_ROOT, root.path())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "FIFO child failed: {status}");
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("opening a swapped FIFO exceeded the bounded child deadline");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn configured_root_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let link_parent = tempdir().unwrap();
        let link = link_parent.path().join("data-link");
        symlink(root.path(), &link).unwrap();
        assert!(matches!(open_root(&link), Err(ClaimError::Unsafe)));
    }

    #[cfg(unix)]
    #[test]
    fn output_directory_swaps_stay_anchored_for_users_backups_and_cleanup() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        fs::create_dir(root.path().join("users")).unwrap();
        fs::create_dir(root.path().join("migration-backups")).unwrap();
        let root_fd = open_root(root.path()).unwrap();
        let users_fd = open_dir_at(&root_fd, std::ffi::OsStr::new("users")).unwrap();
        let backups_fd = open_dir_at(&root_fd, std::ffi::OsStr::new("migration-backups")).unwrap();

        fs::rename(root.path().join("users"), root.path().join("users-saved")).unwrap();
        symlink(outside.path(), root.path().join("users")).unwrap();
        fs::rename(
            root.path().join("migration-backups"),
            root.path().join("migration-backups-saved"),
        )
        .unwrap();
        symlink(outside.path(), root.path().join("migration-backups")).unwrap();

        let target_stage = create_stage_at(&users_fd, ".legacy-stage-user").unwrap();
        let target_file = openat(
            &target_stage,
            "payload",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .unwrap();
        File::from(target_file).write_all(b"user-data").unwrap();
        renameat_with(
            &users_fd,
            ".legacy-stage-user",
            &users_fd,
            "target-user",
            RenameFlags::NOREPLACE,
        )
        .unwrap();

        let backup_stage = create_stage_at(&backups_fd, ".legacy-stage-backup").unwrap();
        let backup_file = openat(
            &backup_stage,
            "payload",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .unwrap();
        File::from(backup_file).write_all(b"backup-data").unwrap();
        renameat_with(
            &backups_fd,
            ".legacy-stage-backup",
            &backups_fd,
            "final-backup",
            RenameFlags::NOREPLACE,
        )
        .unwrap();

        let cleanup_stage = create_stage_at(&backups_fd, ".legacy-stage-cleanup").unwrap();
        let nested_fd = ensure_dir_at(&cleanup_stage, "nested").unwrap();
        let cleanup_file = openat(
            &nested_fd,
            "secret",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .unwrap();
        File::from(cleanup_file).write_all(b"cleanup").unwrap();
        cleanup_stage_at(&backups_fd, ".legacy-stage-cleanup");

        assert_eq!(
            fs::read(root.path().join("users-saved/target-user/payload")).unwrap(),
            b"user-data"
        );
        assert_eq!(
            fs::read(
                root.path()
                    .join("migration-backups-saved/final-backup/payload")
            )
            .unwrap(),
            b"backup-data"
        );
        assert!(!root
            .path()
            .join("migration-backups-saved/.legacy-stage-cleanup")
            .exists());
        assert!(!outside.path().join("target-user").exists());
        assert!(!outside.path().join("final-backup").exists());
        assert!(!outside.path().join(".legacy-stage-cleanup").exists());
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
        let stage_fd = open_root(stage.path()).unwrap();
        let mut budget = CopyBudget {
            start,
            bytes: 0,
            entries: 0,
        };
        let result = copy_tree(
            &fd,
            Path::new("rag"),
            &stage_fd,
            std::ffi::OsStr::new("rag"),
            &mut budget,
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
