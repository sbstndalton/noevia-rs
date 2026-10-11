//! Qualification inputs only; never use this helper to open a live SQLite main database.
use rustix::fs::{fstat, open, FileType, Mode, OFlags};
use std::{fs::File, io::Read, path::Path};
pub fn read(path: &Path, cap: usize) -> Result<Vec<u8>, String> {
    read_with(path, cap, || {})
}
pub fn text(path: &Path, cap: usize) -> Result<String, String> {
    String::from_utf8(read(path, cap)?).map_err(|_| "input UTF8".into())
}
pub fn read_with(
    path: &Path,
    cap: usize,
    mut after_chunk: impl FnMut(),
) -> Result<Vec<u8>, String> {
    let fd = open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| "input open")?;
    let st = fstat(&fd).map_err(|_| "input stat")?;
    if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile
        || st.st_nlink != 1
        || st.st_size < 0
        || st.st_size as u64 > cap as u64
    {
        return Err("input qualification/cap".into());
    }
    let mut file = File::from(fd);
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = file.read(&mut chunk).map_err(|_| "input read")?;
        if n == 0 {
            return Ok(bytes);
        }
        let next = bytes
            .len()
            .checked_add(n)
            .filter(|n| *n <= cap)
            .ok_or("input growth cap")?;
        bytes
            .try_reserve_exact(next - bytes.len())
            .map_err(|_| "input allocation")?;
        bytes.extend_from_slice(&chunk[..n]);
        after_chunk();
    }
}
