//! Is a file in the models folder complete? The model manager lists every `*.gguf` it finds, and a
//! download or copy that is still running leaves one there too. Node registered whatever the
//! manager listed; the move registers a stem only when every file of it passes [`probe`] twice.
//!
//! Nothing outside the root is read and no symbolic link is followed: a link (to a file or a
//! folder) is invisible here, so a stem that exists only behind one is never registered.
use super::Settled;
use std::fs::Metadata;
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
    files: Vec<(PathBuf, u64, i64, u32)>,
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
    let mut found: Vec<PathBuf> = Vec::new();
    let mut temps = false;
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut seen_entries = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|_| "the models folder cannot be read".to_owned())?;
        let mut names: Vec<(String, std::fs::FileType)> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|_| "the models folder cannot be read".to_owned())?;
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
            // `file_type` is the link's own type: a link is neither a file nor a folder here.
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if depth < MAX_DEPTH {
                    stack.push((entry.path(), depth + 1));
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
        // A temp beside a file of this stem: a download or copy in the same folder.
        if names.iter().any(|(n, _)| {
            is_temp(n) && {
                let lower = n.to_ascii_lowercase();
                lower.contains(&stem.to_ascii_lowercase())
            }
        }) {
            temps = true;
        }
        for name in here {
            found.push(dir.join(name));
        }
    }
    if found.is_empty() {
        return Err("no complete file of this name inside the models folder".into());
    }
    if temps {
        return Err("a download or copy of it is still running".into());
    }
    found.sort();
    let mut files = Vec::new();
    for path in found {
        let meta = std::fs::symlink_metadata(&path).map_err(|_| "the file went away".to_owned())?;
        if !meta.file_type().is_file() {
            return Err("not a regular file".into());
        }
        let real = std::fs::canonicalize(&path).map_err(|_| "the file went away".to_owned())?;
        if !real.starts_with(root) {
            return Err("outside the models folder".into());
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
            let mut f =
                std::fs::File::open(&real).map_err(|_| "the file cannot be read".to_owned())?;
            f.read_exact(&mut magic)
                .map_err(|_| "the file cannot be read".to_owned())?;
        }
        if &magic != b"GGUF" {
            return Err("it has no GGUF header yet".into());
        }
        files.push((
            path,
            meta.len(),
            mtime(&meta),
            u32::try_from(meta.nlink()).unwrap_or(u32::MAX),
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
