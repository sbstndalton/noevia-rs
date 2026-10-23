//! Isolated operator preview and new-directory restore of one fully verified workspace archive.
//! The destination is never an application corpus. A failed restore may leave a partial private
//! directory: this module never unlinks a pathname whose owner could have changed in a race.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
use serde_json::{json, Value};
use workspace_restore_verify::VerifiedArchive;

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const FILE_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    FingerprintMismatch,
    InvalidDestination,
    DestinationExists,
    Io,
    Partial,
}

impl Fault {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::FingerprintMismatch => "fingerprint_mismatch",
            Self::InvalidDestination => "destination_invalid",
            Self::DestinationExists => "destination_exists",
            Self::Io => "restore_io",
            Self::Partial => "restore_partial",
        }
    }
}

fn io(_: impl std::fmt::Debug) -> Fault {
    Fault::Io
}

/// The same public fields and JSON-value settings semantics as the Python operator preview.
/// All arrays and integers are already bounded by verify_archive (5000 records, 256 MiB source).
pub(crate) fn preview(archive: &VerifiedArchive) -> Value {
    let listing = archive.listing();
    json!({
        "fingerprint": archive.fingerprint(),
        "fileCount": listing.files.len(),
        "bytes": listing.bytes,
        "files": listing.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        "directories": listing.directories,
        "mode": "new-directory-only",
        "settings": archive.settings(),
    })
}

fn open_absolute_dir(path: &str) -> Result<OwnedFd, Fault> {
    let mut dir = fs::open(Path::new("/"), DIR_FLAGS, Mode::empty()).map_err(io)?;
    for part in path.split('/').filter(|part| !part.is_empty()) {
        dir = fs::openat(&dir, part, DIR_FLAGS, Mode::empty()).map_err(io)?;
    }
    Ok(dir)
}

fn checked_destination(path: &str) -> Result<(OwnedFd, String, String), Fault> {
    // Reject ambiguous lexical components instead of normalizing them. String arguments are
    // UTF-8; archive paths have their own strict safe-key checks in verify_archive.
    if !path.starts_with('/') || path == "/" {
        return Err(Fault::InvalidDestination);
    }
    let parts: Vec<_> = path[1..].split('/').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return Err(Fault::InvalidDestination);
    }
    let (last, parent_parts) = parts.split_last().ok_or(Fault::InvalidDestination)?;
    let parent_path = format!("/{}", parent_parts.join("/"));
    let parent = open_absolute_dir(&parent_path)?;
    let name = (*last).to_string();
    match fs::statat(&parent, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(_) => Err(Fault::DestinationExists),
        Err(rustix::io::Errno::NOENT) => Ok((parent, name, parent_path)),
        Err(e) => Err(io(e)),
    }
}

fn exact_private_dir(fd: &OwnedFd) -> Result<(), Fault> {
    let st = fs::fstat(fd).map_err(io)?;
    if FileType::from_raw_mode(st.st_mode) != FileType::Directory || st.st_mode & 0o777 != 0o700 {
        return Err(Fault::Io);
    }
    Ok(())
}

fn ensure_root_at_name(
    parent: &OwnedFd,
    name: &str,
    parent_path: &str,
    root: &OwnedFd,
) -> Result<(), Fault> {
    let reopened_parent = open_absolute_dir(parent_path)?;
    let parent_stat = fs::fstat(parent).map_err(io)?;
    let reopened_stat = fs::fstat(&reopened_parent).map_err(io)?;
    if (parent_stat.st_dev, parent_stat.st_ino) != (reopened_stat.st_dev, reopened_stat.st_ino) {
        return Err(Fault::Io);
    }
    let root_stat = fs::fstat(root).map_err(io)?;
    let named_stat = fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io)?;
    if FileType::from_raw_mode(named_stat.st_mode) != FileType::Directory
        || (root_stat.st_dev, root_stat.st_ino) != (named_stat.st_dev, named_stat.st_ino)
    {
        return Err(Fault::Io);
    }
    Ok(())
}

fn directory<'a>(
    path: &str,
    owned: &'a mut BTreeMap<String, OwnedFd>,
    root_parent: &OwnedFd,
    root_name: &str,
    parent_path: &str,
    after_mutation: &mut impl FnMut(Stage) -> Result<(), Fault>,
) -> Result<&'a OwnedFd, Fault> {
    let mut prefix = String::new();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        let next = if prefix.is_empty() {
            part.to_string()
        } else {
            format!("{prefix}/{part}")
        };
        if !owned.contains_key(&next) {
            ensure_root_at_name(root_parent, root_name, parent_path, &owned[""])?;
            let parent = owned.get(&prefix).ok_or(Fault::Io)?;
            fs::mkdirat(parent, part, Mode::from_raw_mode(0o700)).map_err(io)?;
            // mkdirat does not return an fd. The immediate no-follow open and mode check reject
            // swapped names; no cleanup is attempted if another actor changes this namespace.
            let child = fs::openat(parent, part, DIR_FLAGS, Mode::empty()).map_err(io)?;
            exact_private_dir(&child)?;
            owned.insert(next.clone(), child);
            after_mutation(Stage::DirectoryCreated)?;
        }
        prefix = next;
    }
    owned.get(&prefix).ok_or(Fault::Io)
}

#[derive(Debug)]
struct CreatedFile {
    parent: String,
    name: String,
    file: std::fs::File,
}

fn ensure_owned_tree(
    parent: &OwnedFd,
    name: &str,
    parent_path: &str,
    dirs: &BTreeMap<String, OwnedFd>,
    files: &[CreatedFile],
) -> Result<(), Fault> {
    ensure_root_at_name(parent, name, parent_path, &dirs[""])?;
    for (path, fd) in dirs.iter().filter(|(path, _)| !path.is_empty()) {
        let (folder, basename) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        let container = dirs.get(folder).ok_or(Fault::Io)?;
        let actual = fs::fstat(fd).map_err(io)?;
        let named = fs::statat(container, basename, AtFlags::SYMLINK_NOFOLLOW).map_err(io)?;
        if FileType::from_raw_mode(named.st_mode) != FileType::Directory
            || named.st_mode & 0o777 != 0o700
            || (actual.st_dev, actual.st_ino) != (named.st_dev, named.st_ino)
        {
            return Err(Fault::Io);
        }
    }
    for created in files {
        let container = dirs.get(&created.parent).ok_or(Fault::Io)?;
        let actual = fs::fstat(&created.file).map_err(io)?;
        let named =
            fs::statat(container, created.name.as_str(), AtFlags::SYMLINK_NOFOLLOW).map_err(io)?;
        if FileType::from_raw_mode(named.st_mode) != FileType::RegularFile
            || named.st_mode & 0o777 != 0o600
            || (actual.st_dev, actual.st_ino) != (named.st_dev, named.st_ino)
        {
            return Err(Fault::Io);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    RootCreated,
    DirectoryCreated,
    FileCreated,
    FileSynced,
    BeforeDirectorySync,
    BeforeParentSync,
}

/// Any failure after mkdirat of the destination is `restore_partial`. Never recursively remove
/// the destination: an identity check followed by unlink can race with a foreign replacement.
pub(crate) fn restore_new(
    archive: &VerifiedArchive,
    destination: &str,
    fingerprint: &str,
) -> Result<Value, Fault> {
    restore_new_with(archive, destination, fingerprint, |_| Ok(()))
}

pub(crate) fn restore_new_with(
    archive: &VerifiedArchive,
    destination: &str,
    fingerprint: &str,
    mut after_mutation: impl FnMut(Stage) -> Result<(), Fault>,
) -> Result<Value, Fault> {
    if fingerprint != archive.fingerprint() {
        return Err(Fault::FingerprintMismatch);
    }
    let (parent, name, parent_path) = checked_destination(destination)?;
    match fs::mkdirat(&parent, name.as_str(), Mode::from_raw_mode(0o700)) {
        Ok(()) => {}
        Err(rustix::io::Errno::EXIST) => return Err(Fault::DestinationExists),
        Err(e) => return Err(io(e)),
    }
    let result: Result<Value, Fault> = (|| {
        let root = fs::openat(&parent, name.as_str(), DIR_FLAGS, Mode::empty()).map_err(io)?;
        exact_private_dir(&root)?;
        let mut owned = BTreeMap::new();
        let mut created_files = Vec::new();
        owned.insert(String::new(), root);
        after_mutation(Stage::RootCreated)?;

        for path in &archive.listing().directories {
            directory(
                path,
                &mut owned,
                &parent,
                &name,
                &parent_path,
                &mut after_mutation,
            )?;
        }
        for (record, bytes) in archive.files() {
            let (folder, basename) = match record.path.rsplit_once('/') {
                Some((folder, basename)) => (folder, basename),
                None => ("", record.path.as_str()),
            };
            directory(
                folder,
                &mut owned,
                &parent,
                &name,
                &parent_path,
                &mut after_mutation,
            )?;
            ensure_owned_tree(&parent, &name, &parent_path, &owned, &created_files)?;
            let dir = owned.get(folder).ok_or(Fault::Io)?;
            let fd =
                fs::openat(dir, basename, FILE_FLAGS, Mode::from_raw_mode(0o600)).map_err(io)?;
            after_mutation(Stage::FileCreated)?;
            let mut file = std::fs::File::from(fd);
            file.write_all(bytes).map_err(io)?;
            // O_CREAT mode is affected by umask; fchmod on our exclusive fd sets exact 0600.
            fs::fchmod(&file, Mode::from_raw_mode(0o600)).map_err(io)?;
            file.sync_all().map_err(io)?;
            created_files.push(CreatedFile {
                parent: folder.to_string(),
                name: basename.to_string(),
                file,
            });
            after_mutation(Stage::FileSynced)?;
        }
        ensure_owned_tree(&parent, &name, &parent_path, &owned, &created_files)?;
        after_mutation(Stage::BeforeDirectorySync)?;
        // Every created directory's entries become durable, including empty directories. A
        // parent may have been renamed; its fd still refers to the directory we opened.
        let mut paths: Vec<_> = owned.keys().collect();
        paths.sort_by_key(|path| std::cmp::Reverse(path.split('/').count()));
        for path in paths {
            fs::fsync(owned.get(path).ok_or(Fault::Io)?).map_err(io)?;
        }
        ensure_owned_tree(&parent, &name, &parent_path, &owned, &created_files)?;
        after_mutation(Stage::BeforeParentSync)?;
        fs::fsync(&parent).map_err(io)?;
        ensure_owned_tree(&parent, &name, &parent_path, &owned, &created_files)?;
        Ok(json!({
            "fileCount": archive.listing().files.len(),
            "bytes": archive.listing().bytes,
            "settings": archive.settings(),
        }))
    })();
    result.map_err(|_| Fault::Partial)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use workspace_restore_verify::verify_archive;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let name = format!(
                "workspace-operator-test-{}-{stamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(name);
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn child(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn archive() -> (Vec<u8>, workspace_restore_verify::VerifiedArchive) {
        let bytes = crate::synthetic_export(b"\0\xff\r\n");
        let verified = verify_archive(&bytes).unwrap();
        (bytes, verified)
    }
    fn path_str(path: &Path) -> &str {
        path.to_str().unwrap()
    }

    #[test]
    fn preview_and_restore_preserve_bytes_settings_and_private_modes() {
        let (body, archive) = archive();
        let preview = preview(&archive);
        assert_eq!(
            preview["fingerprint"],
            workspace_restore_verify::sha256_hex(&body)
        );
        assert_eq!(preview["fileCount"], 1);
        assert_eq!(preview["bytes"], 4);
        assert_eq!(preview["files"], json!(["note.md"]));
        assert_eq!(preview["directories"], json!(["empty"]));
        assert_eq!(preview["settings"], json!({}));
        assert_eq!(preview["mode"], "new-directory-only");
        let root = Sandbox::new();
        let destination = root.child("new");
        let output = restore_new(&archive, path_str(&destination), archive.fingerprint()).unwrap();
        assert_eq!(output, json!({"fileCount":1,"bytes":4,"settings":{}}));
        assert_eq!(
            std::fs::read(destination.join("note.md")).unwrap(),
            b"\0\xff\r\n"
        );
        assert_eq!(
            std::fs::metadata(&destination)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(destination.join("empty"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(destination.join("note.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn mismatch_existing_targets_and_lexical_escape_leave_everything_untouched() {
        let (_, archive) = archive();
        let root = Sandbox::new();
        let destination = root.child("new");
        assert_eq!(
            restore_new(&archive, path_str(&destination), &"0".repeat(64)),
            Err(Fault::FingerprintMismatch)
        );
        assert!(!destination.exists());
        for kind in ["file", "directory", "dangling_symlink"] {
            match kind {
                "file" => std::fs::write(&destination, b"sentinel").unwrap(),
                "directory" => std::fs::create_dir(&destination).unwrap(),
                _ => symlink(root.child("missing"), &destination).unwrap(),
            }
            assert_eq!(
                restore_new(&archive, path_str(&destination), archive.fingerprint()),
                Err(Fault::DestinationExists)
            );
            match kind {
                "file" => {
                    assert_eq!(std::fs::read(&destination).unwrap(), b"sentinel");
                    std::fs::remove_file(&destination).unwrap();
                }
                "directory" => {
                    assert!(destination.is_dir());
                    std::fs::remove_dir(&destination).unwrap();
                }
                _ => {
                    assert!(destination.is_symlink());
                    std::fs::remove_file(&destination).unwrap();
                }
            }
        }
        for value in ["relative", "/tmp/../new", "/tmp/./new", "/tmp//new", "/"] {
            assert_eq!(
                restore_new(&archive, value, archive.fingerprint()),
                Err(Fault::InvalidDestination)
            );
        }
        let external = root.child("external");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("sentinel"), b"safe").unwrap();
        let link = root.child("link");
        symlink(&external, &link).unwrap();
        assert_eq!(
            restore_new(&archive, path_str(&link.join("new")), archive.fingerprint()),
            Err(Fault::Io)
        );
        assert_eq!(std::fs::read(external.join("sentinel")).unwrap(), b"safe");
        assert!(!external.join("new").exists());
    }

    #[test]
    fn injected_write_and_sync_failures_leave_explicit_partial_folder() {
        let (_, archive) = archive();
        for stage_to_fail in [
            Stage::RootCreated,
            Stage::DirectoryCreated,
            Stage::FileCreated,
            Stage::FileSynced,
            Stage::BeforeDirectorySync,
            Stage::BeforeParentSync,
        ] {
            let root = Sandbox::new();
            let destination = root.child("new");
            let result = restore_new_with(
                &archive,
                path_str(&destination),
                archive.fingerprint(),
                |stage| {
                    if stage == stage_to_fail {
                        Err(Fault::Io)
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(result, Err(Fault::Partial), "{stage_to_fail:?}");
            assert!(destination.is_dir());
            assert_eq!(
                std::fs::metadata(&destination)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn parent_destination_child_and_file_substitution_refuse_without_deleting_foreign_data() {
        let (_, archive) = archive();
        for substitution in ["parent", "destination", "child", "file"] {
            let root = Sandbox::new();
            let parent = root.child("parent");
            let foreign = root.child("foreign");
            std::fs::create_dir(&parent).unwrap();
            std::fs::create_dir(&foreign).unwrap();
            std::fs::write(foreign.join("sentinel"), b"foreign-safe").unwrap();
            let destination = parent.join("new");
            let mut swapped = false;
            let result = restore_new_with(
                &archive,
                path_str(&destination),
                archive.fingerprint(),
                |stage| {
                    if swapped {
                        return Ok(());
                    }
                    match (substitution, stage) {
                        ("parent", Stage::RootCreated) => {
                            std::fs::rename(&parent, root.child("parent-original")).unwrap();
                            symlink(&foreign, &parent).unwrap();
                        }
                        ("destination", Stage::RootCreated) => {
                            std::fs::rename(&destination, parent.join("new-original")).unwrap();
                            symlink(&foreign, &destination).unwrap();
                        }
                        ("child", Stage::DirectoryCreated) => {
                            std::fs::rename(
                                destination.join("empty"),
                                destination.join("empty-original"),
                            )
                            .unwrap();
                            symlink(&foreign, destination.join("empty")).unwrap();
                        }
                        ("file", Stage::FileSynced) => {
                            std::fs::rename(
                                destination.join("note.md"),
                                destination.join("note-original"),
                            )
                            .unwrap();
                            symlink(foreign.join("sentinel"), destination.join("note.md")).unwrap();
                        }
                        _ => return Ok(()),
                    }
                    swapped = true;
                    Ok(())
                },
            );
            assert!(swapped, "{substitution}");
            assert_eq!(result, Err(Fault::Partial), "{substitution}");
            assert_eq!(
                std::fs::read(foreign.join("sentinel")).unwrap(),
                b"foreign-safe"
            );
        }
    }
}
