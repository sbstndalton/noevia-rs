//! Bounded, recoverable copy of the pre-multi-user workspace root for its first admin.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
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
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn dir(path: &Path) -> Result<(), ClaimError> {
    match lstat(path)? {
        Some(m) if m.is_dir() && !m.file_type().is_symlink() => Ok(()),
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

fn has_legacy(root: &Path, start: Instant) -> Result<bool, ClaimError> {
    for name in ["projects.json", "providers.json"] {
        if lstat(&root.join(name))?.is_some() {
            return Ok(true);
        }
    }
    for entry in fs::read_dir(root)? {
        check_time(start)?;
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.starts_with("history-") && name.ends_with(".json") {
            return Ok(true);
        }
    }
    Ok(false)
}
fn names(root: &Path, start: Instant) -> Result<Vec<String>, ClaimError> {
    let mut out = Vec::new();
    for entry in fs::read_dir(root)? {
        check_time(start)?;
        let name = entry?.file_name().to_string_lossy().into_owned();
        if [
            "projects.json",
            "providers.json",
            "free-chats.json",
            "auto-roles.json",
        ]
        .contains(&name.as_str())
            || (name.starts_with("history-") && name.ends_with(".json"))
            || name == "rag"
        {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}
fn owner(root: &Path) -> Result<Option<String>, ClaimError> {
    let path = root.join(OWNER);
    let Some(meta) = lstat(&path)? else {
        return Ok(None);
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 4096 {
        return Ok(Some(String::new()));
    }
    let text = fs::read_to_string(path)?;
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
fn previous_owner(root: &Path, start: Instant) -> Result<Option<String>, ClaimError> {
    let users = root.join("users");
    let Some(meta) = lstat(&users)? else {
        return Ok(None);
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(ClaimError::Unsafe);
    }
    let mut scanned = 0usize;
    for entry in fs::read_dir(users)? {
        check_time(start)?;
        scanned += 1;
        if scanned > MAX_ENTRIES {
            return Err(ClaimError::Limit);
        }
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if !uuid(&id) {
            continue;
        }
        let d = entry.path();
        let migration = d.join("migration.json");
        if let (Some(ds), Some(ms)) = (lstat(&d)?, lstat(&migration)?) {
            if ds.is_dir()
                && !ds.file_type().is_symlink()
                && ms.is_file()
                && !ms.file_type().is_symlink()
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
}
fn inspect(root: &Path, candidates: &[String], start: Instant) -> Result<Vec<Item>, ClaimError> {
    fn visit(
        root: &Path,
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
        let path = root.join(rel);
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() || (!meta.is_file() && !meta.is_dir()) {
            return Err(ClaimError::Unsafe);
        }
        if out.len() >= MAX_ENTRIES {
            return Err(ClaimError::Limit);
        }
        if meta.is_file() {
            *bytes = bytes.checked_add(meta.len()).ok_or(ClaimError::Limit)?;
            if *bytes > MAX_BYTES {
                return Err(ClaimError::Limit);
            }
            out.push(Item {
                rel: rel.to_path_buf(),
                dir: false,
                size: meta.len(),
            });
        } else {
            out.push(Item {
                rel: rel.to_path_buf(),
                dir: true,
                size: 0,
            });
            let mut children = fs::read_dir(path)?
                .map(|e| e.map(|x| x.file_name()))
                .collect::<Result<Vec<_>, _>>()?;
            children.sort();
            for child in children {
                visit(root, &rel.join(child), depth + 1, start, out, bytes)?;
            }
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
fn open_read_nofollow(path: &Path) -> Result<File, ClaimError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let f = options.open(path)?;
    if !f.metadata()?.is_file() {
        return Err(ClaimError::Unsafe);
    }
    Ok(f)
}
fn copy_tree(
    root: &Path,
    rel: &Path,
    dest: &Path,
    start: Instant,
    bytes: &mut u64,
) -> Result<(), ClaimError> {
    check_time(start)?;
    let source = root.join(rel);
    let meta = fs::symlink_metadata(&source)?;
    if meta.file_type().is_symlink() {
        return Err(ClaimError::Unsafe);
    }
    if meta.is_dir() {
        fs::create_dir(dest)?;
        set_mode(dest, 0o700)?;
        let mut children = fs::read_dir(source)?
            .map(|e| e.map(|x| x.file_name()))
            .collect::<Result<Vec<_>, _>>()?;
        children.sort();
        for child in children {
            copy_tree(root, &rel.join(&child), &dest.join(child), start, bytes)?;
        }
    } else if meta.is_file() {
        let mut input = open_read_nofollow(&source)?;
        let mut output = OpenOptions::new().write(true).create_new(true).open(dest)?;
        let mut buf = [0u8; 32 * 1024];
        loop {
            check_time(start)?;
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            *bytes = bytes.checked_add(n as u64).ok_or(ClaimError::Limit)?;
            if *bytes > MAX_BYTES {
                return Err(ClaimError::Limit);
            }
            output.write_all(&buf[..n])?;
        }
        output.sync_all()?;
        set_mode(dest, 0o600)?;
    } else {
        return Err(ClaimError::Unsafe);
    }
    Ok(())
}
fn validate(stage: &Path, manifest: &[Item]) -> Result<(), ClaimError> {
    let expected: BTreeMap<PathBuf, &Item> = manifest.iter().map(|i| (i.rel.clone(), i)).collect();
    let mut seen = BTreeSet::new();
    fn visit(
        stage: &Path,
        rel: &Path,
        expected: &BTreeMap<PathBuf, &Item>,
        seen: &mut BTreeSet<PathBuf>,
    ) -> Result<(), ClaimError> {
        let path = stage.join(rel);
        let m = fs::symlink_metadata(&path)?;
        let item = expected.get(rel).ok_or(ClaimError::Unsafe)?;
        if m.file_type().is_symlink()
            || m.is_dir() != item.dir
            || m.is_file() == item.dir
            || (!item.dir && m.len() != item.size)
        {
            return Err(ClaimError::Unsafe);
        }
        seen.insert(rel.to_path_buf());
        if item.dir {
            set_mode(&path, 0o700)?;
            let mut children = fs::read_dir(path)?
                .map(|e| e.map(|x| x.file_name()))
                .collect::<Result<Vec<_>, _>>()?;
            children.sort();
            for child in children {
                visit(stage, &rel.join(child), expected, seen)?;
            }
        } else {
            set_mode(&path, 0o600)?;
        }
        Ok(())
    }
    for item in manifest.iter().filter(|i| i.rel.components().count() == 1) {
        visit(stage, &item.rel, &expected, &mut seen)?;
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
fn write_owner(root: &Path, id: &str) -> Result<(), ClaimError> {
    let path = root.join(OWNER);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    f.write_all(format!(r#"{{"userId":"{id}"}}"#).as_bytes())?;
    f.sync_all()?;
    set_mode(&path, 0o600)?;
    Ok(())
}

/// Claim legacy files for this account. Caller holds the auth DB IMMEDIATE transaction and the
/// dedicated root lock; callers must revalidate an administrator inside that transaction.
pub fn claim_root(root: &Path, user_id: &str) -> Result<bool, ClaimError> {
    if !uuid(user_id) {
        return Err(ClaimError::InvalidUser);
    }
    let start = Instant::now();
    if !has_legacy(root, start)? {
        return Ok(false);
    }
    check_time(start)?;
    let stored = owner(root)?;
    if stored.as_deref() == Some("") {
        return Ok(false);
    }
    let owner_id = match stored.as_ref() {
        Some(id) => id.clone(),
        None => previous_owner(root, start)?.unwrap_or_else(|| user_id.to_string()),
    };
    let target = root.join("users").join(user_id);
    if owner_id != user_id || lstat(&target)?.is_some() {
        if lstat(&root.join(OWNER))?.is_none() {
            let _ = write_owner(root, &owner_id);
        }
        return Ok(false);
    }
    let candidates = names(root, start)?;
    check_time(start)?;
    let manifest = inspect(root, &candidates, start)?;
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
        for name in &candidates {
            check_time(start)?;
            copy_tree(
                root,
                Path::new(name),
                &backup_stage.join(name),
                start,
                &mut backup_bytes,
            )?;
            copy_tree(
                root,
                Path::new(name),
                &target_stage.join(name),
                start,
                &mut target_bytes,
            )?;
        }
        validate(&backup_stage, &manifest)?;
        validate(&target_stage, &manifest)?;
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
            write_owner(root, &owner_id)?;
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
