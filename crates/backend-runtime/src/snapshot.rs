//! Complete SQLite snapshots preserve journal state, workspace receipts and every table.
//! This is an explicit local recovery primitive, not the legacy remote backup manifest.
//! Paths belong to an operator-owned private directory; no HTTP path is accepted here.
use crate::{Control, Fault, Result};
use rusqlite::{
    backup::{Backup, StepResult},
    Connection, OpenFlags,
};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
    time::Duration,
};
pub const MAX_BYTES: u64 = 1024 * 1024 * 1024;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub sha256: String,
    pub bytes: u64,
}
fn new_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|_| Fault::Conflict)
}
fn source(path: &Path) -> Result<File> {
    let fd = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(|_| Fault::Missing)?;
    let file = File::from(fd);
    let metadata = file.metadata().map_err(|_| Fault::Database)?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(Fault::Invalid);
    }
    Ok(file)
}
pub fn digest(path: &Path, control: &Control) -> Result<Snapshot> {
    let mut file = source(path)?;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        control.check()?;
        let n = file.read(&mut buffer).map_err(|_| Fault::Database)?;
        if n == 0 {
            break;
        }
        bytes = bytes.checked_add(n as u64).ok_or(Fault::Capacity)?;
        if bytes > MAX_BYTES {
            return Err(Fault::Capacity);
        }
        hash.update(buffer.get(..n).ok_or(Fault::Database)?);
    }
    Ok(Snapshot {
        sha256: format!("{:x}", hash.finalize()),
        bytes,
    })
}
pub fn create(source_db: &Connection, destination: &Path, control: &Control) -> Result<Snapshot> {
    control.check()?;
    // Resolve only the operator-owned parent (macOS /var is an alias), never
    // the final file. SQLite NOFOLLOW then protects the database name itself.
    let destination_path = destination
        .parent()
        .ok_or(Fault::Invalid)?
        .canonicalize()
        .map_err(|_| Fault::Unavailable)?
        .join(destination.file_name().ok_or(Fault::Invalid)?);
    let destination = destination_path.as_path();
    let file = new_file(destination)?;
    drop(file);
    let page_size: u32 = source_db.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
        return Err(Fault::Database);
    }
    let mut output = Connection::open_with_flags(
        destination,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    output.pragma_update(None, "synchronous", "FULL")?;
    {
        let backup = Backup::new(source_db, &mut output)?;
        loop {
            control.check()?;
            match backup.step(128)? {
                StepResult::Done => break,
                StepResult::More => {}
                StepResult::Busy | StepResult::Locked => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                _ => return Err(Fault::Database),
            }
            let pages = backup.progress().pagecount;
            if pages < 0
                || u64::try_from(pages)
                    .map_err(|_| Fault::Capacity)?
                    .saturating_mul(u64::from(page_size))
                    > MAX_BYTES
            {
                return Err(Fault::Capacity);
            }
        }
    }
    let check: String = output.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(Fault::Database);
    }
    drop(output);
    File::open(destination)
        .and_then(|f| f.sync_all())
        .map_err(|_| Fault::Database)?;
    if let Some(parent) = destination.parent() {
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|_| Fault::Database)?;
    }
    digest(destination, control)
}
/// Restore verified bytes only to a NEW file. Failure leaves an incomplete owned file;
/// the caller must not activate it or silently overwrite it on retry.
pub fn restore(
    source_path: &Path,
    destination: &Path,
    expected: &Snapshot,
    tenant: &str,
    control: &Control,
) -> Result<()> {
    if expected.bytes > MAX_BYTES
        || expected.sha256.len() != 64
        || uuid::Uuid::parse_str(tenant).is_err()
    {
        return Err(Fault::Invalid);
    }
    let destination_path = destination
        .parent()
        .ok_or(Fault::Invalid)?
        .canonicalize()
        .map_err(|_| Fault::Unavailable)?
        .join(destination.file_name().ok_or(Fault::Invalid)?);
    let destination = destination_path.as_path();
    let mut input = source(source_path)?;
    let mut output = new_file(destination)?;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        control.check()?;
        let n = input.read(&mut buffer).map_err(|_| Fault::Database)?;
        if n == 0 {
            break;
        }
        bytes = bytes.checked_add(n as u64).ok_or(Fault::Capacity)?;
        if bytes > expected.bytes {
            return Err(Fault::Capacity);
        }
        let part = buffer.get(..n).ok_or(Fault::Database)?;
        hash.update(part);
        output.write_all(part).map_err(|_| Fault::Database)?;
    }
    if bytes != expected.bytes || format!("{:x}", hash.finalize()) != expected.sha256 {
        return Err(Fault::Conflict);
    }
    output.sync_all().map_err(|_| Fault::Database)?;
    drop(output);
    let restored = Connection::open_with_flags(
        destination,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let check: String = restored.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    let identity: String =
        restored.query_row("SELECT value FROM meta WHERE key='tenant'", [], |r| {
            r.get(0)
        })?;
    if check != "ok" || identity != tenant {
        return Err(Fault::Forbidden);
    }
    control.check()?;
    drop(restored);
    if let Some(parent) = destination.parent() {
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|_| Fault::Database)?;
    }
    Ok(())
}
