//! Is a file in the models folder complete? The model manager lists every `*.gguf` it finds, and a
//! download or copy that is still running leaves one there too. Node registered whatever the
//! manager listed; the move registers a stem only when every file of it passes [`probe`] twice.
//!
//! Nothing outside the root is read and no symbolic link is followed: a link (to a file or a
//! folder) is invisible here, so a stem that exists only behind one is never registered.
use super::Settled;
use rustix::fs::{openat, Mode, OFlags, CWD};
use std::fs::{File, Metadata};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Deepest folder looked into, and the most entries read for one stem.
const MAX_DEPTH: usize = 6;
const MAX_ENTRIES: usize = 50_000;
/// Smallest file with a GGUF header (magic, version, tensor count, metadata count).
const MIN_BYTES: u64 = 24;
/// Suffixes a downloader or copier leaves beside the file it is still writing.
const TEMP_SUFFIXES: [&str; 6] = [
    ".download",
    ".part",
    ".partial",
    ".crdownload",
    ".tmp",
    ".filepart",
];

/// What a probe saw: the files of one stem and how they looked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    files: Vec<(PathBuf, u64, i64, i64, u64, u64)>,
}

impl Seen {
    pub fn files_len(&self) -> usize {
        self.files.len()
    }
}

/// The stem a file name belongs to: its name without `.gguf`, and without a shard suffix
/// (`-00001-of-00003`).
pub fn stem_of(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    let base = lower.strip_suffix(".gguf")?;
    let base = name.get(..base.len())?;
    let bytes = base.as_bytes();
    // "-NNNNN-of-NNNNN"
    let n = bytes.len();
    if n > 15 {
        let tail = bytes.get(n - 15..)?;
        let ok = tail.first() == Some(&b'-')
            && tail.get(1..6)?.iter().all(u8::is_ascii_digit)
            && tail.get(6..10) == Some(b"-of-")
            && tail.get(10..15)?.iter().all(u8::is_ascii_digit);
        if ok {
            return Some(base.get(..n - 15)?.to_owned());
        }
    }
    Some(base.to_owned())
}

fn is_temp(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    TEMP_SUFFIXES.iter().any(|s| lower.ends_with(s))
        // rsync: ".name.gguf.AbC123"
        || (lower.starts_with('.') && lower.contains(".gguf."))
}

fn mtime(meta: &Metadata) -> i64 {
    meta.mtime()
}

/// Looks at `stem`'s files under `root` (already canonical). `Ok(Seen)` when every file looks
/// complete now; `Err(why)` otherwise (a reason for the log, never a path outside the root).
pub fn probe(root: &Path, stem: &str, now: SystemTime, settle: Duration) -> Result<Seen, String> {
    probe_with_hook(root, stem, now, settle, &|_| {})
}

// Opens every root component without following a link, then uses only opened directory fds.
fn root_dir(root: &Path) -> Result<File, String> {
    let mut dir = File::from(
        openat(
            CWD,
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| "the models folder cannot be read")?,
    );
    for component in root.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                dir = File::from(
                    openat(
                        &dir,
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(|_| "the models folder cannot be read")?,
                );
            }
            _ => return Err("invalid models root".into()),
        }
    }
    Ok(dir)
}

fn fd_path(file: &File) -> PathBuf {
    use std::os::fd::AsRawFd;
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

pub(super) fn probe_with_hook(
    root: &Path,
    stem: &str,
    now: SystemTime,
    settle: Duration,
    before_open: &impl Fn(&Path),
) -> Result<Seen, String> {
    let mut found: Vec<(PathBuf, File)> = Vec::new();
    let mut temps = false;
    let mut stack = vec![(root_dir(root)?, PathBuf::new(), 0)];
    let mut seen_entries = 0usize;
    while let Some((dir, relative, depth)) = stack.pop() {
        let entries =
            std::fs::read_dir(fd_path(&dir)).map_err(|_| "the models folder cannot be read")?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|_| "the models folder cannot be read")?;
            seen_entries += 1;
            if seen_entries > MAX_ENTRIES {
                return Err("the models folder is too large to check".into());
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = relative.join(&name);
            if kind.is_dir() {
                if depth < MAX_DEPTH {
                    before_open(&path);
                    let child = openat(
                        &dir,
                        name.as_str(),
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(|_| "the folder changed during the check")?;
                    stack.push((File::from(child), path, depth + 1));
                }
                continue;
            }
            names.push((name, kind));
        }
        let here: Vec<&String> = names
            .iter()
            .filter(|(n, k)| k.is_file() && stem_of(n).as_deref() == Some(stem))
            .map(|(n, _)| n)
            .collect();
        if here.is_empty() {
            continue;
        }
        if names
            .iter()
            .any(|(n, _)| is_temp(n) && n.to_ascii_lowercase().contains(&stem.to_ascii_lowercase()))
        {
            temps = true;
        }
        for name in here {
            let path = relative.join(name);
            before_open(&path);
            // NONBLOCK prevents a regular-file-to-FIFO swap from blocking before fstat.
            let file = openat(
                &dir,
                name.as_str(),
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| "the file changed during the check")?;
            found.push((path, File::from(file)));
        }
    }
    if found.is_empty() {
        return Err("no complete file of this name inside the models folder".into());
    }
    if temps {
        return Err("a download or copy of it is still running".into());
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    let mut files = Vec::new();
    for (path, mut file) in found {
        let meta = file.metadata().map_err(|_| "the file went away")?;
        if !meta.file_type().is_file() {
            return Err("not a regular file".into());
        }
        if meta.len() < MIN_BYTES {
            return Err("too small to hold a GGUF header".into());
        }
        let modified = meta
            .modified()
            .map_err(|_| "no modification time".to_owned())?;
        match now.duration_since(modified) {
            Ok(age) if age >= settle => {}
            _ => return Err("changed too recently".into()),
        }
        let mut magic = [0u8; 4];
        {
            use std::io::Read;
            file.read_exact(&mut magic)
                .map_err(|_| "the file cannot be read".to_owned())?;
        }
        if &magic != b"GGUF" {
            return Err("it has no GGUF header yet".into());
        }
        let after = file.metadata().map_err(|_| "the file went away")?;
        if after.len() != meta.len()
            || after.mtime() != meta.mtime()
            || after.mtime_nsec() != meta.mtime_nsec()
        {
            return Err("it is still changing".into());
        }
        files.push((
            path,
            meta.len(),
            mtime(&meta),
            meta.mtime_nsec(),
            meta.dev(),
            meta.ino(),
        ));
    }
    Ok(Seen { files })
}

/// Two probes some seconds apart saw the same files in the same state.
pub fn stable(first: &Result<Seen, String>, second: &Result<Seen, String>) -> Settled {
    match (first, second) {
        (Ok(a), Ok(b)) if a == b => Settled::Yes,
        (Ok(_), Ok(_)) => Settled::Not("it is still changing".into()),
        (Err(why), _) | (_, Err(why)) => Settled::Not(why.clone()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod races {
    use super::*;
    #[test]
    fn replacing_a_file_with_identical_size_and_mtime_changes_identity() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("m.gguf");
        let bytes = [b"GGUF".as_slice(), &[0; 60]].concat();
        std::fs::write(&path, &bytes).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let first = probe(root.path(), "m", SystemTime::now(), Duration::ZERO);
        std::fs::rename(&path, root.path().join("old")).unwrap();
        std::fs::write(&path, bytes).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let second = probe(root.path(), "m", SystemTime::now(), Duration::ZERO);
        assert!(first.is_ok() && second.is_ok());
        assert_ne!(first, second);
    }

    #[test]
    fn swapped_directory_and_file_symlinks_are_refused_before_read() {
        for directory in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            std::fs::create_dir(root.path().join("sub")).unwrap();
            std::fs::write(
                root.path().join("sub/m.gguf"),
                [b"GGUF".as_slice(), &[0; 60]].concat(),
            )
            .unwrap();
            std::fs::write(
                outside.path().join("m.gguf"),
                [b"GGUF".as_slice(), &[0; 60]].concat(),
            )
            .unwrap();
            let swapped = std::sync::atomic::AtomicBool::new(false);
            let result = probe_with_hook(
                root.path(),
                "m",
                SystemTime::now(),
                Duration::ZERO,
                &|relative| {
                    let wanted = if directory {
                        Path::new("sub")
                    } else {
                        Path::new("sub/m.gguf")
                    };
                    if relative == wanted
                        && !swapped.swap(true, std::sync::atomic::Ordering::SeqCst)
                    {
                        let path = root.path().join(relative);
                        std::fs::rename(&path, path.with_extension("old")).unwrap();
                        let target = if directory {
                            outside.path().to_path_buf()
                        } else {
                            outside.path().join("m.gguf")
                        };
                        std::os::unix::fs::symlink(target, path).unwrap();
                    }
                },
            );
            assert!(swapped.load(std::sync::atomic::Ordering::SeqCst));
            assert!(result.is_err(), "swapped link accepted: {result:?}");
        }
    }
}
