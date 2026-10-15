//! Local explicitly selected snapshot files only; not a discovery/import API.
use rustix::fs::{self, FileType, Mode, OFlags};
use std::{fs::File, io::Read, path::Path};
pub const CAP: usize = 4 * 1024 * 1024;
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Path,
    Open,
    Type,
    Read,
    Allocation,
    Cap,
}
pub fn open(path: &Path) -> Result<File, Error> {
    if !path.is_absolute() || path.as_os_str().len() > 4096 {
        return Err(Error::Path);
    }
    let fd = fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| Error::Open)?;
    let stat = fs::fstat(&fd).map_err(|_| Error::Type)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1 {
        return Err(Error::Type);
    }
    Ok(File::from(fd))
}
pub fn read(path: &Path) -> Result<Vec<u8>, Error> {
    let mut file = open(path)?;
    consume(&mut file)
}
pub fn consume(file: &mut File) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    out.try_reserve_exact(CAP).map_err(|_| Error::Allocation)?;
    let mut scratch = [0; 4096];
    loop {
        let remaining = CAP - out.len();
        let take = scratch.len().min(remaining.saturating_add(1));
        let n = file
            .read(scratch.get_mut(..take).ok_or(Error::Read)?)
            .map_err(|_| Error::Read)?;
        if n == 0 {
            return Ok(out);
        }
        if n > remaining {
            return Err(Error::Cap);
        }
        out.extend_from_slice(scratch.get(..n).ok_or(Error::Read)?);
    }
}
