//! Descriptor-relative, bounded tenant file access. No path is reopened after validation.
use crate::{input::Json, normalize, Setting, BODY_LIMIT, FILE};
use rustix::fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags};
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::{AsFd, OwnedFd},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
#[derive(Debug)]
pub enum Fault {
    Unavailable,
    Cancelled,
    Busy,
}
impl From<rustix::io::Errno> for Fault {
    fn from(_: rustix::io::Errno) -> Self {
        Self::Unavailable
    }
}
impl From<std::io::Error> for Fault {
    fn from(_: std::io::Error) -> Self {
        Self::Unavailable
    }
}
#[derive(Debug)]
pub struct Root {
    fd: OwnedFd,
}
fn cancelled(c: &AtomicBool) -> Result<(), Fault> {
    if c.load(Ordering::Acquire) {
        Err(Fault::Cancelled)
    } else {
        Ok(())
    }
}
fn regular(fd: impl AsFd) -> Result<fs::Stat, Fault> {
    let stat = fs::fstat(fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1 {
        return Err(Fault::Unavailable);
    }
    Ok(stat)
}
fn dir(
    parent: impl AsFd,
    name: &str,
    create: bool,
    c: &AtomicBool,
) -> Result<Option<OwnedFd>, Fault> {
    cancelled(c)?;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match fs::openat(parent.as_fd(), name, flags, Mode::empty()) {
        Ok(fd) => Ok(Some(fd)),
        Err(rustix::io::Errno::NOENT) if create => {
            cancelled(c)?;
            match fs::mkdirat(parent.as_fd(), name, Mode::from_raw_mode(0o700)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(e) => return Err(e.into()),
            }
            Ok(Some(fs::openat(parent, name, flags, Mode::empty())?))
        }
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
impl Root {
    pub fn open(path: &Path) -> Result<Self, Fault> {
        Ok(Self {
            fd: fs::open(
                path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?,
        })
    }
    fn tenant(&self, id: &str, create: bool, c: &AtomicBool) -> Result<Option<OwnedFd>, Fault> {
        if id.len() != 36 || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
            return Err(Fault::Unavailable);
        }
        let Some(users) = dir(&self.fd, "users", create, c)? else {
            return Ok(None);
        };
        dir(users, id, create, c)
    }
    pub fn read(&self, id: &str, c: &AtomicBool) -> Result<Setting, Fault> {
        let Some(tenant) = self.tenant(id, false, c)? else {
            return Ok(normalize(None));
        };
        let fd = match fs::openat(
            &tenant,
            FILE,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(normalize(None)),
            Err(e) => return Err(e.into()),
        };
        let stat = regular(&fd)?;
        if stat.st_size < 0 || stat.st_size > BODY_LIMIT as i64 {
            return Err(Fault::Unavailable);
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve(BODY_LIMIT + 1)
            .map_err(|_| Fault::Unavailable)?;
        File::from(fd)
            .take((BODY_LIMIT + 1) as u64)
            .read_to_end(&mut bytes)?;
        cancelled(c)?;
        if bytes.len() > BODY_LIMIT {
            return Err(Fault::Unavailable);
        }
        // Depth excess is unsafe state rather than ordinary malformed JSON fallback.
        let parsed = Json::parse(&bytes);
        if parsed.is_err() && depth_excess(&bytes) {
            return Err(Fault::Unavailable);
        }
        Ok(normalize(parsed.as_ref().ok()))
    }
    pub fn write(
        &self,
        id: &str,
        value: &Setting,
        _cap: server_store::RustRoutingMode,
        c: &AtomicBool,
    ) -> Result<(), Fault> {
        let tenant = self.tenant(id, true, c)?.ok_or(Fault::Unavailable)?;
        let lock = fs::openat(
            &tenant,
            "routing-mode.lock",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        regular(&lock)?;
        let start = Instant::now();
        loop {
            cancelled(c)?;
            match fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::WOULDBLOCK)
                    if start.elapsed() < Duration::from_millis(100) =>
                {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(rustix::io::Errno::WOULDBLOCK) => return Err(Fault::Busy),
                Err(e) => return Err(e.into()),
            }
        }
        check_target(&tenant)?;
        cancelled(c)?;
        let tmp = "routing-mode.json.tmp";
        let fd = fs::openat(
            &tenant,
            tmp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|e| {
            if e == rustix::io::Errno::EXIST {
                Fault::Busy
            } else {
                e.into()
            }
        })?;
        let own = regular(&fd)?;
        let mut file = File::from(fd);
        let result = (|| {
            cancelled(c)?;
            file.write_all(value.bytes().as_bytes())?;
            file.sync_all()?;
            cancelled(c)?;
            check_target(&tenant)?;
            let current = fs::statat(&tenant, tmp, AtFlags::SYMLINK_NOFOLLOW)?;
            if current.st_ino != own.st_ino
                || current.st_dev != own.st_dev
                || FileType::from_raw_mode(current.st_mode) != FileType::RegularFile
                || current.st_nlink != 1
            {
                return Err(Fault::Unavailable);
            }
            fs::renameat(&tenant, tmp, &tenant, FILE)?;
            // A sync error after rename is a committed write with an unavailable response.
            fs::fsync(&tenant)?;
            Ok(())
        })();
        if result.is_err() {
            // Never remove an entry that has already visibly replaced our temporary inode.
            if fs::statat(&tenant, tmp, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(|s| {
                s.st_ino == own.st_ino
                    && s.st_dev == own.st_dev
                    && s.st_nlink == 1
                    && FileType::from_raw_mode(s.st_mode) == FileType::RegularFile
            }) {
                let _ = fs::unlinkat(&tenant, tmp, AtFlags::empty());
            }
        }
        result
    }
}
fn check_target(tenant: impl AsFd) -> Result<(), Fault> {
    match fs::statat(tenant, FILE, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(s) if FileType::from_raw_mode(s.st_mode) == FileType::RegularFile && s.st_nlink == 1 => {
            Ok(())
        }
        Err(rustix::io::Errno::NOENT) => Ok(()),
        _ => Err(Fault::Unavailable),
    }
}
fn depth_excess(bytes: &[u8]) -> bool {
    let (mut depth, mut quote, mut escape) = (0usize, false, false);
    for &b in bytes {
        if quote {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                quote = false;
            }
        } else if b == b'"' {
            quote = true;
        } else if b == b'[' || b == b'{' {
            depth += 1;
            if depth > 128 {
                return true;
            }
        } else if b == b']' || b == b'}' {
            depth = depth.saturating_sub(1);
        }
    }
    false
}
