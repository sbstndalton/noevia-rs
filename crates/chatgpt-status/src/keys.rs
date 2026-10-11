use crate::{Error, PLAIN_BYTES, ROW_BYTES, USER_BYTES};
use rustix::fs::{fstat, open, openat, FileType, Mode, OFlags};
use std::{
    os::fd::OwnedFd,
    path::{Component, Path},
};
use zeroize::Zeroizing;
/// Root is an fd capability, with every path component opened without following a symlink.
pub struct Root {
    pub(crate) fd: OwnedFd,
}
impl Root {
    pub fn open(path: &Path) -> Result<Self, Error> {
        if !path.is_absolute() || path.as_os_str().len() > 4096 {
            return Err(Error::Unavailable);
        }
        let mut fd = open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| Error::Unavailable)?;
        let mut count = 0;
        for component in path.components() {
            match component {
                Component::RootDir => {}
                Component::Normal(name) => {
                    count += 1;
                    if count > 128 {
                        return Err(Error::Limit);
                    }
                    fd = openat(
                        &fd,
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(|_| Error::Unavailable)?
                }
                _ => return Err(Error::Unavailable),
            }
        }
        Ok(Self { fd })
    }
    pub(crate) fn regular(&self, name: &str) -> Result<Option<OwnedFd>, Error> {
        let fd = match openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(_) => return Err(Error::Unavailable),
        };
        let stat = fstat(&fd).map_err(|_| Error::Unavailable)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(Error::Unavailable);
        }
        Ok(Some(fd))
    }
    fn key(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>, Error> {
        let Some(fd) = self.regular(name)? else {
            return Ok(None);
        };
        let before = fstat(&fd).map_err(|_| Error::Unavailable)?;
        if before.st_size != 32 {
            return Err(Error::Unavailable);
        }
        let mut bytes = Zeroizing::new([0; 33]);
        let mut n = 0;
        let mut attempts = 0;
        loop {
            attempts += 1;
            if attempts > 64 {
                return Err(Error::Unavailable);
            }
            let space = bytes.get_mut(n..).ok_or(Error::Unavailable)?;
            if space.is_empty() {
                return Err(Error::Unavailable);
            }
            match rustix::io::read(&fd, space) {
                Ok(0) => break,
                Ok(read) => n += read,
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => return Err(Error::Unavailable),
            }
        }
        let after = fstat(&fd).map_err(|_| Error::Unavailable)?;
        if n != 32
            || before.st_size != after.st_size
            || before.st_mtime != after.st_mtime
            || before.st_mtime_nsec != after.st_mtime_nsec
            || before.st_ctime != after.st_ctime
            || before.st_ctime_nsec != after.st_ctime_nsec
        {
            return Err(Error::Unavailable);
        }
        Ok(Some(Zeroizing::new(
            bytes.get(..32).ok_or(Error::Unavailable)?.to_vec(),
        )))
    }
}
/// Snapshot taken once at service startup, exactly as actual createSecretStore.
/// Debug deliberately excludes key bytes; replacing files requires coordinated restart.
pub struct StatusKeys {
    current: Zeroizing<Vec<u8>>,
    previous: Option<Zeroizing<Vec<u8>>>,
}
impl std::fmt::Debug for StatusKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StatusKeys").finish_non_exhaustive()
    }
}
impl StatusKeys {
    pub fn load(root: &Root, env: Option<&str>) -> Result<Self, Error> {
        let current = root.key("secrets.key")?.ok_or(Error::Unavailable)?;
        let previous = match root.key("secrets.key.previous")? {
            Some(p) => Some(p),
            None => match env {
                Some(s) if !s.is_empty() => {
                    if s.len() > 4096 {
                        return Err(Error::Limit);
                    }
                    let units: Zeroizing<Vec<u16>> = Zeroizing::new(s.encode_utf16().collect());
                    let trimmed = policy_leaves::js_trim(&units);
                    let bytes = if trimmed.len() == 64
                        && trimmed.iter().all(|u| {
                            char::from_u32(u32::from(*u)).is_some_and(|c| c.is_ascii_hexdigit())
                        }) {
                        let mut bytes = Vec::with_capacity(32);
                        for pair in trimmed.as_chunks::<2>().0 {
                            let hi = pair
                                .first()
                                .and_then(|u| char::from_u32(u32::from(*u)))
                                .and_then(|c| c.to_digit(16))
                                .ok_or(Error::Unavailable)?;
                            let lo = pair
                                .get(1)
                                .and_then(|u| char::from_u32(u32::from(*u)))
                                .and_then(|c| c.to_digit(16))
                                .ok_or(Error::Unavailable)?;
                            bytes.push(((hi << 4) | lo) as u8)
                        }
                        bytes
                    } else {
                        secret_envelope::decode_base64_node(trimmed)
                    };
                    let bytes = Zeroizing::new(bytes);
                    if bytes.len() != 32 {
                        return Err(Error::Unavailable);
                    }
                    Some(bytes)
                }
                _ => None,
            },
        };
        let previous = previous.filter(|p| p.as_slice() != current.as_slice());
        Ok(Self { current, previous })
    }
    pub(crate) fn open(&self, text: &str, user: &str) -> Result<Zeroizing<String>, Error> {
        if text.len() > ROW_BYTES || user.len() > USER_BYTES {
            return Err(Error::Limit);
        }
        let units: Zeroizing<Vec<u16>> = Zeroizing::new(text.encode_utf16().collect());
        let (used, plain) = secret_envelope::open(
            &self.current,
            self.previous.as_deref().map(Vec::as_slice),
            &units,
            Some(user.as_bytes()),
        )
        .map_err(|_| Error::Unavailable)?;
        if used == secret_envelope::KeyUsed::None {
            if text.len() > PLAIN_BYTES {
                return Err(Error::Limit);
            }
            return Ok(Zeroizing::new(text.to_owned()));
        }
        if plain.len() > PLAIN_BYTES {
            return Err(Error::Limit);
        }
        let text = Zeroizing::new(String::from_utf8_lossy(&plain).into_owned());
        if text.len() > PLAIN_BYTES {
            return Err(Error::Limit);
        }
        Ok(text)
    }
}
