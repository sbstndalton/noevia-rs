//! Kernel-enforced beneath-root opens. No ambient reopen after validation.
use super::{Budget, Error, INI_CAP};
use rustix::fd::OwnedFd;
#[cfg(target_os = "linux")]
use rustix::fs::{open, Mode, OFlags};
use std::{
    fs::File,
    io::Read,
    path::{Component, Path},
};

pub struct Root {
    fd: OwnedFd,
    #[cfg(test)]
    fixture: Option<std::path::PathBuf>,
}
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
enum Opened {
    Found(File),
    Missing,
    /// EACCES/EPERM opening it; callers decide whether that refuses.
    Denied,
}
pub struct Entry {
    pub name: String,
    pub directory: bool,
}
impl Root {
    pub fn new(path: &Path) -> Result<Self, Error> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = path;
            Err(Error::Refused)
        }
        #[cfg(target_os = "linux")]
        {
            Ok(Self {
                fd: open(
                    path,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| Error::Refused)?,
                #[cfg(test)]
                fixture: None,
            })
        }
    }
    #[cfg(test)]
    pub fn synthetic(path: &Path) -> Result<Self, Error> {
        Ok(Self {
            fd: rustix::fs::open(
                path,
                rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::DIRECTORY,
                rustix::fs::Mode::empty(),
            )
            .map_err(|_| Error::Refused)?,
            fixture: Some(path.to_owned()),
        })
    }
    pub fn file(
        &self,
        relative: &str,
        directory: bool,
        budget: &mut Budget,
    ) -> Result<Option<File>, Error> {
        match self.open(relative, directory, budget)? {
            Opened::Found(file) => Ok(Some(file)),
            Opened::Missing => Ok(None),
            Opened::Denied => Err(Error::Refused),
        }
    }
    /// Like [`Root::entries`], but a folder the process may not open (EACCES/EPERM) is
    /// `Ok(None)`, as `os.walk` skips it. Escapes and every other error still refuse.
    #[cfg(feature = "native-model-updates")]
    pub fn entries_unless_denied(
        &self,
        relative: &str,
        budget: &mut Budget,
    ) -> Result<Option<Vec<Entry>>, Error> {
        match self.open(relative, true, budget)? {
            Opened::Found(file) => self.list(file, relative, budget).map(Some),
            Opened::Missing => Ok(Some(Vec::new())),
            Opened::Denied => Ok(None),
        }
    }
    fn open(&self, relative: &str, directory: bool, budget: &mut Budget) -> Result<Opened, Error> {
        budget.work()?;
        if relative.len() > 4096
            || relative.contains('\0')
            || Path::new(relative)
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(Error::Refused);
        }
        #[cfg(test)]
        if let Some(root) = &self.fixture {
            // Synthetic semantic adapter only; not containment evidence or production code.
            let file = match File::open(root.join(relative)) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Opened::Missing),
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    return Ok(Opened::Denied)
                }
                Err(_) => return Err(Error::Refused),
            };
            let metadata = file.metadata().map_err(|_| Error::Refused)?;
            if directory != metadata.is_dir() || (!directory && !metadata.is_file()) {
                return Err(Error::Refused);
            }
            return Ok(Opened::Found(file));
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (&self.fd, directory);
            Err(Error::Refused)
        }
        #[cfg(target_os = "linux")]
        {
            use rustix::fs::{openat2, ResolveFlags};
            let mut flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
            if directory {
                flags |= OFlags::DIRECTORY | OFlags::NOFOLLOW;
            }
            let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS;
            let mut result = openat2(&self.fd, relative, flags, Mode::empty(), resolve);
            if matches!(result, Err(rustix::io::Errno::AGAIN)) {
                budget.work()?;
                result = openat2(&self.fd, relative, flags, Mode::empty(), resolve);
            }
            let fd = match result {
                Ok(fd) => fd,
                Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => {
                    return Ok(Opened::Missing)
                }
                Err(rustix::io::Errno::ACCESS | rustix::io::Errno::PERM) => {
                    return Ok(Opened::Denied)
                }
                Err(_) => return Err(Error::Refused),
            };
            let file = File::from(fd);
            let metadata = file.metadata().map_err(|_| Error::Refused)?;
            if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
                return Err(Error::Refused);
            }
            Ok(Opened::Found(file))
        }
    }
    pub fn entries(&self, relative: &str, budget: &mut Budget) -> Result<Vec<Entry>, Error> {
        let Some(file) = self.file(relative, true, budget)? else {
            return Ok(Vec::new());
        };
        self.list(file, relative, budget)
    }
    fn list(&self, file: File, relative: &str, budget: &mut Budget) -> Result<Vec<Entry>, Error> {
        let directory = rustix::fs::Dir::new(file).map_err(|_| Error::Refused)?;
        let mut entries = Vec::new();
        for entry in directory {
            // Includes dot/skipped/non-GGUF entries: cap before filtering or sorting.
            budget.entry()?;
            let entry = entry.map_err(|_| Error::Refused)?;
            let name = entry.file_name().to_str().map_err(|_| Error::Refused)?;
            if name == "." || name == ".." {
                continue;
            }
            if name.len() > 256 {
                return Err(Error::Refused);
            }
            entries.try_reserve(1).map_err(|_| Error::Refused)?;
            let directory = if entry.file_type() == rustix::fs::FileType::Unknown {
                let path = if relative == "." {
                    name.to_owned()
                } else {
                    format!("{relative}/{name}")
                };
                self.stat_impl(&path, budget, true)?
                    .is_some_and(|m| m.is_dir())
            } else {
                entry.file_type() == rustix::fs::FileType::Directory
            };
            entries.push(Entry {
                name: name.to_owned(),
                directory,
            });
        }
        Ok(entries)
    }
    pub fn stat(
        &self,
        relative: &str,
        budget: &mut Budget,
    ) -> Result<Option<std::fs::Metadata>, Error> {
        self.stat_impl(relative, budget, false)
    }
    fn stat_impl(
        &self,
        relative: &str,
        budget: &mut Budget,
        nofollow: bool,
    ) -> Result<Option<std::fs::Metadata>, Error> {
        budget.work()?;
        if relative.len() > 4096
            || relative.contains('\0')
            || Path::new(relative)
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(Error::Refused);
        }
        #[cfg(test)]
        if let Some(root) = &self.fixture {
            return match if nofollow {
                std::fs::symlink_metadata(root.join(relative))
            } else {
                std::fs::metadata(root.join(relative))
            } {
                Ok(stat) => Ok(Some(stat)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(_) => Err(Error::Refused),
            };
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = nofollow;
            Err(Error::Refused)
        }
        #[cfg(target_os = "linux")]
        {
            let flags = OFlags::PATH
                | OFlags::CLOEXEC
                | if nofollow {
                    OFlags::NOFOLLOW
                } else {
                    OFlags::empty()
                };
            let fd = match rustix::fs::openat2(
                &self.fd,
                relative,
                flags,
                Mode::empty(),
                rustix::fs::ResolveFlags::BENEATH | rustix::fs::ResolveFlags::NO_MAGICLINKS,
            ) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => return Ok(None),
                Err(_) => return Err(Error::Refused),
            };
            Ok(Some(File::from(fd).metadata().map_err(|_| Error::Refused)?))
        }
    }
    pub fn ini(&self, name: &str, budget: &mut Budget) -> Result<Vec<u8>, Error> {
        let Some(file) = self.file(name, false, budget)? else {
            return Ok(Vec::new());
        };
        if file.metadata().map_err(|_| Error::Refused)?.len() > INI_CAP as u64 {
            return Err(Error::Refused);
        }
        let mut bytes = Vec::new();
        bytes.try_reserve(INI_CAP + 1).map_err(|_| Error::Refused)?;
        file.take(INI_CAP as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Refused)?;
        if bytes.len() > INI_CAP {
            return Err(Error::Refused);
        }
        budget.work()?;
        Ok(bytes)
    }
}
