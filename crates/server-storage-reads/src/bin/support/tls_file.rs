//! Fixed synthetic PKI file reader, not a production credential loader.
use rustix::fs::{FileType, Mode, OFlags};
use std::{fs::File, io::Read, path::Path};
pub const FILE_CAP: usize = 65536;
pub fn read(path: &Path) -> Result<Vec<u8>, String> {
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| "fixture file open refused")?;
    let stat = rustix::fs::fstat(&fd).map_err(|_| "fixture file type refused")?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_nlink != 1
        || stat.st_mode & 0o777 != 0o600
    {
        return Err("fixture file type/mode refused".into());
    }
    let mut file = File::from(fd);
    let mut out = Vec::new();
    out.try_reserve_exact(FILE_CAP)
        .map_err(|_| "fixture allocation refused")?;
    let mut scratch = [0; 4096];
    loop {
        let remaining = FILE_CAP - out.len();
        let take = scratch.len().min(remaining + 1);
        let n = file
            .read(scratch.get_mut(..take).ok_or("fixture IO refused")?)
            .map_err(|_| "fixture IO refused")?;
        if n == 0 {
            return Ok(out);
        }
        if n > remaining {
            return Err("fixture file cap refused".into());
        }
        out.extend_from_slice(scratch.get(..n).ok_or("fixture IO refused")?);
    }
}
