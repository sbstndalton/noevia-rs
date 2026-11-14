//! Reviewed Git-export source check: the native port of noevia-core `code-sandbox/verify-native-source.py`.
//!
//! The digest binds every path, kind, executable bit and content of one archive root, independent of tar
//! owner/time and gzip metadata. Nothing is written before the whole archive validated and its digest
//! matched the reviewed one. The reader follows the reference's archive semantics (pax global and extended
//! headers, GNU long names, the ustar prefix field, the first all-zero block ends the archive) and refuses
//! where the reference would silently stop reading: see [`tar`].

#![forbid(unsafe_code)]

mod json;
pub mod tar;

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

/// Domain separator hashed before the canonical record list.
pub const DOMAIN: &[u8] = b"noevia-reviewed-git-export-v1\0";

/// Size and count bounds. [`Limits::DEFAULT`] are the reference's constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest compressed input.
    pub max_archive: u64,
    /// Largest uncompressed input or expanded stream; also the largest single file.
    pub max_expanded: u64,
    /// Most archive members, the root directory included.
    pub max_entries: usize,
    /// Largest sum of regular file sizes.
    pub max_content: u64,
}

impl Limits {
    pub const DEFAULT: Limits = Limits {
        max_archive: 64 * 1024 * 1024,
        max_expanded: 128 * 1024 * 1024,
        max_entries: 20_000,
        max_content: 128 * 1024 * 1024,
    };
}

impl Default for Limits {
    fn default() -> Self {
        Limits::DEFAULT
    }
}

/// A refusal. The text is the reference's `ValueError` text where it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused(pub String);

impl Refused {
    pub(crate) fn new(text: impl Into<String>) -> Self {
        Refused(text.into())
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// One validated entry below the archive root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Directory,
    /// `exec` is the header mode masked with 0o111.
    File { exec: u32, content: Vec<u8> },
    Symlink { target: String },
}

/// A validated archive: its digest and its entries by relative name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inspected {
    pub digest: String,
    pub entries: BTreeMap<String, Entry>,
}

/// The reference's `path_ok`: a relative, canonical (pathlib-normalised) POSIX path of at most 4096 bytes
/// without NUL or `..`. A lone `.` is canonical there and is accepted here as well.
pub fn path_ok(name: &str) -> Result<(), Refused> {
    let bad = || Refused::new("noncanonical source path");
    if name.is_empty() || name.len() > 4096 || name.contains('\0') || name.starts_with('/') {
        return Err(bad());
    }
    if name == "." {
        return Ok(());
    }
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(bad());
        }
    }
    Ok(())
}

/// Parent directories of a relative canonical name, nearest last (`a/b/c` gives `a`, `a/b`).
fn parents(name: &str) -> impl Iterator<Item = &str> {
    name.match_indices('/').filter_map(move |(at, _)| name.get(..at))
}

/// Bounded read of an expanded gzip stream: at most `limit + 1` bytes, so an over-limit stream is detected
/// without expanding it further. Members are read back to back, zero padding between and after members is
/// skipped, and any other trailing byte is refused, as Python's `gzip` reader does.
pub fn gunzip_bounded(data: &[u8], limit: u64) -> Result<Vec<u8>, Refused> {
    let cap = limit.saturating_add(1);
    let mut out = Vec::new();
    let mut rest = data;
    loop {
        let mut decoder = flate2::bufread::GzDecoder::new(rest);
        let room = cap.saturating_sub(out.len() as u64);
        let mut taken = (&mut decoder).take(room);
        taken.read_to_end(&mut out).map_err(|_| Refused::new("invalid compressed source"))?;
        if out.len() as u64 >= cap {
            break;
        }
        rest = decoder.into_inner();
        let skip = rest.iter().take_while(|byte| **byte == 0).count();
        rest = rest.get(skip..).unwrap_or_default();
        if rest.is_empty() {
            break;
        }
    }
    if out.len() as u64 > limit {
        return Err(Refused::new("source size limit"));
    }
    Ok(out)
}

/// The reference's `inspect`: validate the archive and compute its digest. `prefix` is the single archive
/// root directory name (for a codeload export, `<repository>-<ref>`).
pub fn inspect(data: &[u8], prefix: &str, limits: &Limits) -> Result<Inspected, Refused> {
    path_ok(prefix)?;
    if prefix.contains('/') {
        return Err(Refused::new("invalid source root"));
    }
    let compressed = data.starts_with(&[0x1f, 0x8b]);
    let bound = if compressed { limits.max_archive } else { limits.max_expanded };
    if data.len() as u64 > bound {
        return Err(Refused::new("source size limit"));
    }
    let expanded_owned;
    let expanded: &[u8] = if compressed {
        expanded_owned = gunzip_bounded(data, limits.max_expanded)?;
        &expanded_owned
    } else {
        data
    };
    if expanded.len() as u64 > limits.max_expanded {
        return Err(Refused::new("expanded source size limit"));
    }

    let root_dir = format!("{prefix}/");
    let mut root_seen = false;
    let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
    let mut logical_bytes: u64 = 0;
    let mut reader = tar::Reader::new(expanded);
    while let Some(member) = reader.next_member()? {
        if entries.len() + usize::from(root_seen) >= limits.max_entries {
            return Err(Refused::new("source entry limit"));
        }
        let raw = String::from_utf8(member.name.clone()).map_err(|_| Refused::new("invalid source name encoding"))?;
        let full = if member.kind == tar::Kind::Directory { raw.trim_end_matches('/').to_owned() } else { raw };
        if full == prefix && member.kind == tar::Kind::Directory {
            if root_seen {
                return Err(Refused::new("duplicate source root"));
            }
            root_seen = true;
            continue;
        }
        let Some(name) = full.strip_prefix(&root_dir) else {
            return Err(Refused::new("unexpected source root"));
        };
        path_ok(name)?;
        if entries.contains_key(name) {
            return Err(Refused::new("duplicate source entry"));
        }
        if member.sparse {
            return Err(Refused::new("sparse source entry refused"));
        }
        let entry = match member.kind {
            tar::Kind::File { plain } => {
                if !plain {
                    return Err(Refused::new("unsupported regular source encoding"));
                }
                let size = u64::try_from(member.size)
                    .ok()
                    .filter(|size| *size <= limits.max_expanded)
                    .ok_or_else(|| Refused::new("source file size"))?;
                logical_bytes = logical_bytes.saturating_add(size);
                if logical_bytes > limits.max_content {
                    return Err(Refused::new("aggregate source content limit"));
                }
                let content = reader.content(&member)?;
                if content.len() as u64 != size {
                    return Err(Refused::new("truncated source file"));
                }
                Entry::File { exec: (member.mode & 0o111) as u32, content: content.to_vec() }
            }
            tar::Kind::Directory => Entry::Directory,
            tar::Kind::Symlink => {
                let target = String::from_utf8(member.linkname.clone())
                    .map_err(|_| Refused::new("invalid source link encoding"))?;
                if target.is_empty() || target.len() > 4096 || target.contains('\0') || target.starts_with('/') {
                    return Err(Refused::new("unsafe source link"));
                }
                let mut depth = parents(name).count();
                for part in target.split('/') {
                    if part == ".." {
                        depth = depth.checked_sub(1).ok_or_else(|| Refused::new("escaping source link"))?;
                    } else if !part.is_empty() && part != "." {
                        depth += 1;
                    }
                }
                Entry::Symlink { target }
            }
            tar::Kind::Other => return Err(Refused::new("unsupported source type")),
        };
        entries.insert(name.to_owned(), entry);
    }
    if entries.is_empty() {
        return Err(Refused::new("empty source archive"));
    }
    for name in entries.keys() {
        for parent in parents(name) {
            if entries.get(parent) != Some(&Entry::Directory) {
                return Err(Refused::new("missing or non-directory source parent"));
            }
        }
    }
    let digest = digest_of(&entries);
    Ok(Inspected { digest, entries })
}

/// sha256 of the domain and the canonical JSON record list, sorted by UTF-8 bytes (BTreeMap<String> order).
fn digest_of(entries: &BTreeMap<String, Entry>) -> String {
    let mut canonical = String::from("[");
    for (index, (name, entry)) in entries.iter().enumerate() {
        if index > 0 {
            canonical.push(',');
        }
        canonical.push('[');
        json::push_str(&mut canonical, name);
        match entry {
            Entry::Directory => canonical.push_str(",\"directory\""),
            Entry::File { exec, content } => {
                canonical.push_str(&format!(",\"file\",{exec},{},\"{}\"", content.len(), hex(&Sha256::digest(content))));
            }
            Entry::Symlink { target } => {
                canonical.push_str(",\"symlink\",");
                json::push_str(&mut canonical, target);
            }
        }
        canonical.push(']');
    }
    canonical.push(']');
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    hasher.update(canonical.as_bytes());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// True for 64 lowercase hex digits.
pub fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// True for 40 lowercase hex digits (a full Git commit id).
pub fn is_ref(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// The reference's `extract`: refuse unless the archive's digest equals `expected`, then write it into the
/// fresh directory `destination` (created 0700; its parent must exist). Directories are made shallowest
/// first, files are created exclusively and set to 0755 or 0644, links are made last and never followed.
pub fn extract(data: &[u8], prefix: &str, expected: &str, destination: &Path, limits: &Limits) -> Result<String, Error> {
    if !is_digest(expected) {
        return Err(Refused::new("invalid reviewed digest").into());
    }
    let inspected = inspect(data, prefix, limits)?;
    if inspected.digest != expected {
        return Err(Refused::new("source differs from reviewed Git export").into());
    }
    write_tree(&inspected.entries, destination)?;
    Ok(inspected.digest)
}

fn write_tree(entries: &BTreeMap<String, Entry>, destination: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new().mode(0o700).create(destination)?;
    let mut order: Vec<&String> = entries.keys().collect();
    order.sort_by(|a, b| (a.matches('/').count(), a.as_str()).cmp(&(b.matches('/').count(), b.as_str())));
    for name in &order {
        let path = destination.join(name);
        match entries.get(*name) {
            Some(Entry::Directory) => std::fs::DirBuilder::new().mode(0o755).create(&path)?,
            Some(Entry::File { exec, content }) => {
                let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
                std::io::Write::write_all(&mut file, content)?;
                drop(file);
                let mode = if *exec != 0 { 0o755 } else { 0o644 };
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
            }
            Some(Entry::Symlink { .. }) | None => {}
        }
    }
    for (name, entry) in entries {
        if let Entry::Symlink { target } = entry {
            std::os::unix::fs::symlink(target, destination.join(name))?;
        }
    }
    Ok(())
}

/// A refusal or an I/O failure while writing the verified tree.
#[derive(Debug)]
pub enum Error {
    Refused(Refused),
    Io(std::io::Error),
}

impl From<Refused> for Error {
    fn from(value: Refused) -> Self {
        Error::Refused(value)
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Error::Io(value)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Refused(refused) => refused.fmt(f),
            Error::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

/// Read at most `limit + 1` bytes of `reader`; refuse when more than `limit` arrive (the reference's `bounded`).
pub fn read_bounded(reader: impl Read, limit: u64) -> Result<Vec<u8>, Error> {
    let mut data = Vec::new();
    reader.take(limit.saturating_add(1)).read_to_end(&mut data)?;
    if data.len() as u64 > limit {
        return Err(Refused::new("source size limit").into());
    }
    Ok(data)
}
