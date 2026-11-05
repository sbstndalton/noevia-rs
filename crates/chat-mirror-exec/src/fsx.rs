//! Local writes for the per-user index and the brain store: temp file then rename, in the user's
//! own workspace directory, never through a symlink.

use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// `mkdirSync(dir, { recursive: true, mode })`, refusing a symlink at `dir` itself.
pub fn ensure_dir(dir: &Path, mode: u32) -> Result<(), String> {
    if let Ok(m) = std::fs::symlink_metadata(dir) {
        if m.file_type().is_symlink() {
            return Err("refused: the folder is a link".into());
        }
        if m.is_dir() {
            return Ok(());
        }
        return Err("refused: not a folder".into());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(dir)
        .map_err(|e| format!("mkdir: {}", e.kind()))
}

/// Writes `bytes` to `file` atomically: a fresh `<file>.<pid>.tmp` (mode 0600, created new, so an
/// existing link there is never followed), flushed, then renamed over `file` (a rename replaces a
/// link at `file`, never its target). A failure leaves `file` as it was.
pub fn write_atomic(file: &Path, bytes: &[u8]) -> Result<(), String> {
    let name = file
        .file_name()
        .ok_or("refused: no file name")?
        .to_string_lossy()
        .into_owned();
    let tmp = file.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    // A stale temp file from a crashed run (same pid after a restart is possible) goes first.
    if std::fs::symlink_metadata(&tmp).is_ok() {
        std::fs::remove_file(&tmp).map_err(|e| format!("stale temp: {}", e.kind()))?;
    }
    let done = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc_nofollow())
            .open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, file)
    })();
    if let Err(e) = done {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("write: {}", e.kind()));
    }
    Ok(())
}

/// O_NOFOLLOW (create_new already refuses an existing link; this also covers the race).
const fn libc_nofollow() -> i32 {
    #[cfg(target_os = "linux")]
    {
        0o400_000
    }
    #[cfg(not(target_os = "linux"))]
    {
        0x100
    }
}

/// Reads a small file without following a link at the file itself; `None` if missing, linked or
/// not a regular file.
pub fn read_plain(file: &Path) -> Option<Vec<u8>> {
    let m = std::fs::symlink_metadata(file).ok()?;
    if !m.is_file() {
        return None;
    }
    std::fs::read(file).ok()
}
