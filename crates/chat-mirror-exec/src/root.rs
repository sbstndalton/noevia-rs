//! The account's own workspace folder, held open: every local read, write and removal of the
//! executors goes through descriptors opened from it, one path component at a time, never
//! following a link (review 2026-11-05, P1 confinement).
//!
//! [`UserRoot::open`] opens `<data>/users` and then `<id>` with `O_NOFOLLOW | O_DIRECTORY`, so a
//! link at either is refused rather than followed, and keeps the account folder's descriptor. A
//! later swap of `users/<id>` (to a link to another account, or to another folder) cannot redirect
//! anything: each operation is relative to the descriptor opened first. Each operation also
//! re-checks that `users/<id>` is still that same folder (device and inode, not followed), so a
//! swapped or removed account fails closed instead of carrying on in a detached folder.
//!
//! Leaves: a read opens the file with `O_NOFOLLOW` and inspects the opened handle (a regular
//! file, its size); a write goes to a fresh temp file (`O_CREAT | O_EXCL | O_NOFOLLOW`, 0600),
//! fsynced, then `renameat` within the same held folder, then the folder is fsynced.

use rustix::fs::{self as rfs, AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

static SEQ: AtomicU64 = AtomicU64::new(0);

fn kind(e: Errno) -> String {
    format!("{:?}", std::io::Error::from(e).kind())
}

/// One plain name: not empty, not `.`/`..`, no `/` or NUL.
fn plain(name: &str) -> Result<&str, String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err("refused: not a plain name".into());
    }
    Ok(name)
}

/// The account folder, held open.
#[derive(Debug)]
pub struct UserRoot {
    users: OwnedFd,
    name: String,
    fd: OwnedFd,
    dev: u64,
    ino: u64,
}

/// What a read found.
#[derive(Debug, PartialEq, Eq)]
pub enum Leaf {
    /// Nothing there (the file or its folder).
    Missing,
    /// A regular file's bytes, and whether it was larger than the limit (then the bytes are
    /// only the first `limit + 1`).
    File { bytes: Vec<u8>, over: bool },
}

impl UserRoot {
    /// Opens `<data_dir>/users/<name>` without following a link at `users` or `<name>`.
    pub fn open(data_dir: &Path, name: &str) -> Result<Self, String> {
        let name = plain(name)?.to_owned();
        let base = rfs::open(
            data_dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| format!("data folder: {}", kind(e)))?;
        let users = rfs::openat(&base, "users", DIR, Mode::empty())
            .map_err(|e| format!("refused: users folder: {}", kind(e)))?;
        let fd = rfs::openat(&users, name.as_str(), DIR, Mode::empty())
            .map_err(|e| format!("refused: account folder: {}", kind(e)))?;
        let st = rfs::fstat(&fd).map_err(|e| format!("account folder: {}", kind(e)))?;
        #[allow(clippy::useless_conversion, clippy::cast_sign_loss)]
        let (dev, ino) = (st.st_dev as u64, st.st_ino as u64);
        Ok(Self {
            users,
            name,
            fd,
            dev,
            ino,
        })
    }

    /// `users/<id>` is still the folder this root holds (not followed: a link there fails).
    pub fn verify(&self) -> Result<(), String> {
        let st = rfs::statat(&self.users, self.name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|e| format!("refused: account folder changed: {}", kind(e)))?;
        #[allow(clippy::useless_conversion, clippy::cast_sign_loss)]
        let same = FileType::from_raw_mode(st.st_mode) == FileType::Directory
            && st.st_dev as u64 == self.dev
            && st.st_ino as u64 == self.ino;
        if same {
            Ok(())
        } else {
            Err("refused: account folder changed".into())
        }
    }

    /// The folder for `sub` (`None` = the account folder itself): `Ok(None)` when it is missing
    /// and `create` is `None`; made with `create`'s mode otherwise. A link there is refused.
    fn folder(&self, sub: Option<&str>, create: Option<u32>) -> Result<Option<OwnedFd>, String> {
        let Some(sub) = sub else {
            return rfs::openat(&self.fd, ".", DIR, Mode::empty())
                .map(Some)
                .map_err(|e| format!("account folder: {}", kind(e)));
        };
        let sub = plain(sub)?;
        if let Some(mode) = create {
            match rfs::mkdirat(&self.fd, sub, Mode::from_raw_mode(mode as _)) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(e) => return Err(format!("mkdir: {}", kind(e))),
            }
        }
        match rfs::openat(&self.fd, sub, DIR, Mode::empty()) {
            Ok(fd) => Ok(Some(fd)),
            Err(Errno::NOENT) if create.is_none() => Ok(None),
            Err(Errno::LOOP | Errno::NOTDIR) => Err("refused: the folder is a link".into()),
            Err(e) => Err(format!("folder: {}", kind(e))),
        }
    }

    /// Reads `sub/name` (at most `limit + 1` bytes). A link or a non-regular file there is an
    /// error, never followed; a missing file or folder is [`Leaf::Missing`].
    pub fn read(&self, sub: Option<&str>, name: &str, limit: u64) -> Result<Leaf, String> {
        self.verify()?;
        let name = plain(name)?;
        let Some(dir) = self.folder(sub, None)? else {
            return Ok(Leaf::Missing);
        };
        let fd = match rfs::openat(
            &dir,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(Leaf::Missing),
            Err(Errno::LOOP) => return Err("refused: the file is a link".into()),
            Err(e) => return Err(format!("read: {}", kind(e))),
        };
        let st = rfs::fstat(&fd).map_err(|e| format!("read: {}", kind(e)))?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            return Err("refused: not a regular file".into());
        }
        let mut bytes = Vec::new();
        File::from(fd)
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|e| format!("read: {:?}", e.kind()))?;
        let over = u64::try_from(bytes.len()).unwrap_or(u64::MAX) > limit;
        Ok(Leaf::File { bytes, over })
    }

    /// Replaces `sub/name` with `bytes` atomically (mode 0600); `sub` is made with `dir_mode`
    /// when missing. A failure leaves the old file whole.
    pub fn write(
        &self,
        sub: Option<&str>,
        dir_mode: u32,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        self.verify()?;
        let name = plain(name)?;
        let dir = self
            .folder(sub, Some(dir_mode))?
            .ok_or("refused: no folder")?;
        let tmp = format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let done = (|| -> Result<(), String> {
            let fd = rfs::openat(
                &dir,
                tmp.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|e| format!("write: {}", kind(e)))?;
            rfs::fchmod(&fd, Mode::from_raw_mode(0o600))
                .map_err(|e| format!("write: {}", kind(e)))?;
            let mut f = File::from(fd);
            f.write_all(bytes)
                .and_then(|()| f.sync_all())
                .map_err(|e| format!("write: {:?}", e.kind()))?;
            // The account may have been swapped while the temp file was written: the rename
            // stays in the held folder either way, but a swapped account fails here.
            self.verify()?;
            rfs::renameat(&dir, tmp.as_str(), &dir, name)
                .map_err(|e| format!("write: {}", kind(e)))?;
            let _ = rfs::fsync(&dir);
            Ok(())
        })();
        if done.is_err() {
            let _ = rfs::unlinkat(&dir, tmp.as_str(), AtFlags::empty());
        }
        done
    }

    /// Removes `sub/name` if it is there (a link there is removed itself, never its target).
    pub fn remove(&self, sub: Option<&str>, name: &str) -> Result<(), String> {
        self.verify()?;
        let name = plain(name)?;
        let Some(dir) = self.folder(sub, None)? else {
            return Ok(());
        };
        match rfs::unlinkat(&dir, name, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(format!("remove: {}", kind(e))),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn tree(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "cmx-root-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(d.join("users/a")).unwrap();
        std::fs::create_dir_all(d.join("users/b")).unwrap();
        d
    }

    #[test]
    fn round_trip_and_refusals() {
        let d = tree("rt");
        let r = UserRoot::open(&d, "a").unwrap();
        assert_eq!(r.read(None, "x.json", 10).unwrap(), Leaf::Missing);
        r.write(Some("sub"), 0o700, "x.json", b"{}").unwrap();
        assert_eq!(
            r.read(Some("sub"), "x.json", 10).unwrap(),
            Leaf::File {
                bytes: b"{}".to_vec(),
                over: false
            }
        );
        std::os::unix::fs::symlink(d.join("users/b"), d.join("users/a/l")).unwrap();
        assert!(r.read(Some("l"), "x.json", 10).is_err());
        std::os::unix::fs::symlink(d.join("users/b/y"), d.join("users/a/y")).unwrap();
        assert!(r.read(None, "y", 10).is_err());
        assert!(UserRoot::open(&d, "..").is_err());
        std::os::unix::fs::symlink(d.join("users/b"), d.join("users/c")).unwrap();
        assert!(UserRoot::open(&d, "c").is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
