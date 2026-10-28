//! Private native database files. Only operator-owned paths enter this module.
use crate::{Fault, Result};
use rusqlite::{Connection, OpenFlags};
use std::{
    fs::{File, OpenOptions},
    path::Path,
};
#[derive(Clone, Copy)]
pub enum Open {
    Existing,
    New,
    ExistingOrNew,
}
pub fn open(path: &Path, mode: Open) -> Result<Connection> {
    // Resolve the configured directory, not the filename (macOS /var is an alias).
    let parent = path
        .parent()
        .ok_or(Fault::Invalid)?
        .canonicalize()
        .map_err(|_| Fault::Unavailable)?;
    let path = parent.join(path.file_name().ok_or(Fault::Invalid)?);
    if !matches!(mode, Open::Existing) {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => file.sync_all().map_err(|_| Fault::Database)?,
            Err(e)
                if matches!(mode, Open::ExistingOrNew)
                    && e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Err(Fault::Conflict),
            Err(_) => return Err(Fault::Database),
        }
    }
    let fd = rustix::fs::open(
        &path,
        rustix::fs::OFlags::RDWR
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| Fault::Database)?;
    let file = File::from(fd);
    let metadata = file.metadata().map_err(|_| Fault::Database)?;
    if !metadata.is_file() {
        return Err(Fault::Forbidden);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(Fault::Forbidden);
        }
    }
    let db = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let current = std::fs::symlink_metadata(&path).map_err(|_| Fault::Database)?;
        if current.dev() != metadata.dev() || current.ino() != metadata.ino() || !current.is_file()
        {
            return Err(Fault::Conflict);
        }
    }
    db.pragma_update(None, "synchronous", "FULL")?;
    File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|_| Fault::Database)?;
    Ok(db)
}
