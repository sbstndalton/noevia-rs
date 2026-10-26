//! Diary workspace archive verification, a port of noevia-services'
//! `diary/agent/workspace_restore.py` `_verify` (noevia#982). It is the gate in front of the
//! isolated operator restore, the ZIP restore preview and the in-app ZIP import: an untrusted
//! upload of up to 272 MiB is accepted only when it is exactly a `noevia-workspace-v1` export.
//! Nothing here touches the filesystem or the network. Digest-only verification remains compatible;
//! verify_archive also exposes checked contents/settings for callers that must not parse ZIP again.
//!
//! Every check `_verify` makes is made here, in the same order, with the same refusal class
//! (`Reject::code`, mapped back to Python's message by the Diary):
//!
//! 1. archive larger than 272 MiB (`archive_too_large`);
//! 2. a corrupt container: no EOCD, bad central directory, undecodable names, extract version
//!    above 6.3, malformed extra fields (`invalid`);
//! 3. more than 10,002 entries or a duplicate entry name (`entries`);
//! 4. per entry, in directory order: the name minus trailing `/` must be a safe relative key, no
//!    leading `/`, no `\`, no NUL, no empty, `.` or `..` segment (`path`); its Unix file type must
//!    be none, regular or directory (symlinks, devices, FIFOs, sockets refused) and it must not
//!    be encrypted (`entry_type`); declared size at most 64 MiB and method stored or DEFLATE
//!    (`entry_limits`); directory entries empty (`directory_not_empty`); a directory mode only on
//!    a `/` name and a regular mode never on one (`type_mismatch`);
//! 5. declared sizes summing to more than 264 MiB (`expanded`) — the size-bomb guard, checked
//!    before anything is inflated;
//! 6. `manifest.json` present (`invalid`) and at most 8 MiB declared (`manifest_size`), valid
//!    UTF-8 JSON (`invalid`), an object (`manifest_object`), `format` `noevia-workspace-v1`
//!    (`format`), `files` and `directories` lists of at most 5,000 (`counts`);
//! 7. each directory a safe key (`path`), not repeated (`duplicate_directory`);
//! 8. each file record an object with a non-negative integer `bytes` and a string `sha256`
//!    (`file_record`), a safe `path` (`path`, or `invalid` when missing) not already a file or
//!    directory (`overlap`), whose `workspace/<path>` entry exists (`invalid`), reads cleanly
//!    (`invalid`) and has exactly that size and lowercase SHA-256 (`checksum`);
//! 9. no file or directory below a file (`parent_overlap`);
//! 10. the entry names exactly `manifest.json`, `workspace/`, each listed file and each listed
//!     directory with `/` (`entries_mismatch`: nothing missing, nothing unlisted);
//! 11. integer `fileCount` and `bytes` (`totals_invalid`) equal to the real count and size, the
//!     size at most 256 MiB (`totals_mismatch`).
//!
//! Stricter than Python (each refused as `invalid`; none occurs in an export the Diary writes):
//! - ZIP: an archive comment, a zip64 locator or zip64 / Info-ZIP Unicode Path extra fields,
//!   0xFFFFFFFF sizes or offsets, more than one disk; bytes before the first member, between
//!   members or after the central directory; overlapping members; a central directory whose
//!   count differs from the EOCD's; any local header that disagrees with the central directory
//!   (Python checks only the members it opens, and only signature and name); flag bits other
//!   than encryption, DEFLATE options, data descriptor and UTF-8; empty names or names with NUL
//!   (Python truncates them); a stored member whose two sizes differ; a DEFLATE stream that does
//!   not end exactly at the compressed size or yields more or less than the declared size
//!   (Python ignores trailing input and stops at the declared size before the CRC check). Every
//!   member is checked even when Python would refuse for a later reason first, so a doubly
//!   broken archive may be `invalid` here and a more specific class in Python.
//! - JSON: the manifest is decoded like `json.loads(bytes)` (UTF-8 with or without BOM, UTF-16,
//!   UTF-32), but Python's `json` also accepts `NaN` / `Infinity`, numbers that
//!   overflow a double, lone or UTF-8-encoded surrogates (it decodes
//!   with `surrogatepass`) and nesting up to its recursion limit; serde_json
//!   refuses these. Integers beyond 64 bits are `file_record` / `totals_invalid` here where
//!   Python, with unbounded integers, reaches `checksum` / `totals_mismatch`.
//!
//! Python bugs this port surfaced (fixed in the Diary in the same change): a corrupt DEFLATE
//! stream (`zlib.error`), a newer extract version (`NotImplementedError`) and a deeply nested
//! manifest (`RecursionError`) escaped `verify` as server errors instead of a refusal.

#![forbid(unsafe_code)]

mod crc;
mod decode;
mod zip;

pub use decode::{DECODE_V1_END, DECODE_V1_MAGIC, MAX_DECODE_METADATA};

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

/// workspace_export.MAX_ARCHIVE / MAX_FILE / MAX_TOTAL, unchanged.
pub const MAX_ARCHIVE: usize = 272 * 1024 * 1024;
pub const MAX_FILE: u64 = 64 * 1024 * 1024;
pub const MAX_TOTAL: u64 = 256 * 1024 * 1024;
/// Declared expanded size allowed: the files plus 8 MiB for the manifest.
pub const MAX_EXPANDED: u64 = MAX_TOTAL + 8 * 1024 * 1024;
pub const MAX_MANIFEST: u64 = 8 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 10_002;
pub const MAX_RECORDS: usize = 5000;
pub const FORMAT: &str = "noevia-workspace-v1";
/// Leading line of the canonical listing that `listing_sha256` hashes.
pub const LISTING_LABEL: &str = "noevia-workspace-verify-v1";

const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

/// Why an archive was refused: one variant per message Python raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    ArchiveTooLarge,
    Invalid,
    Entries,
    Path,
    EntryType,
    EntryLimits,
    DirectoryNotEmpty,
    TypeMismatch,
    Expanded,
    ManifestSize,
    ManifestObject,
    Format,
    Counts,
    DuplicateDirectory,
    FileRecord,
    Overlap,
    Checksum,
    ParentOverlap,
    EntriesMismatch,
    TotalsInvalid,
    TotalsMismatch,
}

impl Reject {
    /// The stable code the CLI prints and the Diary maps back to its message.
    pub fn code(self) -> &'static str {
        match self {
            Reject::ArchiveTooLarge => "archive_too_large",
            Reject::Invalid => "invalid",
            Reject::Entries => "entries",
            Reject::Path => "path",
            Reject::EntryType => "entry_type",
            Reject::EntryLimits => "entry_limits",
            Reject::DirectoryNotEmpty => "directory_not_empty",
            Reject::TypeMismatch => "type_mismatch",
            Reject::Expanded => "expanded",
            Reject::ManifestSize => "manifest_size",
            Reject::ManifestObject => "manifest_object",
            Reject::Format => "format",
            Reject::Counts => "counts",
            Reject::DuplicateDirectory => "duplicate_directory",
            Reject::FileRecord => "file_record",
            Reject::Overlap => "overlap",
            Reject::Checksum => "checksum",
            Reject::ParentOverlap => "parent_overlap",
            Reject::EntriesMismatch => "entries_mismatch",
            Reject::TotalsInvalid => "totals_invalid",
            Reject::TotalsMismatch => "totals_mismatch",
        }
    }
}

/// One verified file: its path under `workspace/`, size and (checked) SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

/// What a verified archive restores: files and directories sorted by path, and the total size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub files: Vec<VerifiedFile>,
    pub directories: Vec<String>,
    pub bytes: u64,
}

impl Verified {
    /// SHA-256 (hex) of the canonical listing: `LISTING_LABEL`, then per file
    /// `F\0path\0bytes\0sha256`, per directory `D\0path`, then `T\0count\0bytes`, every record
    /// ended by `\0` and paths in code point order. No safe path contains NUL, so the encoding
    /// is unambiguous. The Diary computes the same from Python's result and requires equality.
    pub fn listing_sha256(&self) -> String {
        let mut h = Sha256::new();
        h.update(LISTING_LABEL.as_bytes());
        h.update(b"\0");
        for f in &self.files {
            h.update(format!("F\0{}\0{}\0{}\0", f.path, f.bytes, f.sha256).as_bytes());
        }
        for d in &self.directories {
            h.update(format!("D\0{d}\0").as_bytes());
        }
        h.update(format!("T\0{}\0{}\0", self.files.len(), self.bytes).as_bytes());
        hex(&h.finalize())
    }
}

/// A fully verified archive. Contents and settings become accessible only after ALL checks
/// succeed. Immutable accessors keep the listing, data and fingerprint bound together.
pub struct VerifiedArchive {
    listing: Verified,
    settings: Value,
    fingerprint: String,
    contents: Vec<Vec<u8>>,
}

impl VerifiedArchive {
    pub fn listing(&self) -> &Verified {
        &self.listing
    }

    /// The manifest's settings value, or an empty object when absent (Python's default).
    /// This is intentionally not restricted to an object: existing archives allow any JSON value.
    pub fn settings(&self) -> &Value {
        &self.settings
    }

    /// SHA-256 of the exact original archive bytes, not a reserialized manifest.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Files in the same sorted order as listing().files; each byte sequence passed CRC,
    /// declared-size and manifest SHA-256 checks. No ZIP parsing by callers is required.
    pub fn files(&self) -> impl ExactSizeIterator<Item = (&VerifiedFile, &[u8])> {
        self.listing
            .files
            .iter()
            .zip(self.contents.iter().map(Vec::as_slice))
    }
}

/// Validate once and retain the verified file bytes and settings. Bounded by MAX_ARCHIVE,
/// MAX_TOTAL and MAX_MANIFEST; source bytes are never base64 encoded or inflated a second time.
pub fn verify_archive(body: &[u8]) -> Result<VerifiedArchive, Reject> {
    verify_inner(body, true)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Lowercase hex SHA-256, as Python's `hashlib.sha256(data).hexdigest()`.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

pub use crc::crc32;

/// managed_storage.safe_key: a relative key with no leading `/`, no `\`, no NUL and no empty,
/// `.` or `..` segment.
pub fn safe_key(path: &str) -> bool {
    !(path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == ".."))
}

/// Python's `type(v) is int` (bool excluded) as a 64-bit value; None otherwise.
fn int(v: Option<&Value>) -> Option<i128> {
    let n = v?.as_number()?;
    n.as_i64()
        .map(i128::from)
        .or_else(|| n.as_u64().map(i128::from))
}

/// `json.loads(bytes)`'s decoding: `json.detect_encoding` (BOMs, then the NUL pattern of the
/// first bytes) and a strict decode. None when the bytes do not decode (Python raises).
fn json_text(b: &[u8]) -> Option<String> {
    let units16 = |b: &[u8], be: bool| -> Option<String> {
        if !b.len().is_multiple_of(2) {
            return None;
        }
        let units: Vec<u16> = b
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&p| {
                if be {
                    u16::from_be_bytes(p)
                } else {
                    u16::from_le_bytes(p)
                }
            })
            .collect();
        String::from_utf16(&units).ok()
    };
    let units32 = |b: &[u8], be: bool| -> Option<String> {
        if !b.len().is_multiple_of(4) {
            return None;
        }
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|&a| {
                char::from_u32(if be {
                    u32::from_be_bytes(a)
                } else {
                    u32::from_le_bytes(a)
                })
            })
            .collect()
    };
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let tail = |n: usize| b.get(n..).unwrap_or_default();
    if b.starts_with(b"\x00\x00\xfe\xff") {
        return units32(tail(4), true);
    }
    if b.starts_with(b"\xff\xfe\x00\x00") {
        return units32(tail(4), false);
    }
    if b.starts_with(b"\xfe\xff") {
        return units16(tail(2), true);
    }
    if b.starts_with(b"\xff\xfe") {
        return units16(tail(2), false);
    }
    if b.starts_with(b"\xef\xbb\xbf") {
        return std::str::from_utf8(tail(3)).ok().map(str::to_owned);
    }
    if b.len() >= 4 {
        if at(0) == 0 {
            return if at(1) != 0 {
                units16(b, true)
            } else {
                units32(b, true)
            };
        }
        if at(1) == 0 {
            return if at(2) != 0 || at(3) != 0 {
                units16(b, false)
            } else {
                units32(b, false)
            };
        }
    } else if b.len() == 2 {
        if at(0) == 0 {
            return units16(b, true);
        }
        if at(1) == 0 {
            return units16(b, false);
        }
    }
    std::str::from_utf8(b).ok().map(str::to_owned)
}

/// Verify `body` like `_verify`; see the crate docs for every check.
pub fn verify(body: &[u8]) -> Result<Verified, Reject> {
    // Use the identical validation path without retaining all source bytes for digest-only users.
    verify_inner(body, false).map(|archive| archive.listing)
}

fn verify_inner(body: &[u8], retain: bool) -> Result<VerifiedArchive, Reject> {
    if body.len() > MAX_ARCHIVE {
        return Err(Reject::ArchiveTooLarge);
    }
    let (members, cd_offset) = zip::directory(body)?;
    let mut names: HashSet<&str> = HashSet::new();
    if members.len() > MAX_ENTRIES || !members.iter().all(|m| names.insert(m.name.as_str())) {
        return Err(Reject::Entries);
    }
    let mut total: u64 = 0;
    for m in &members {
        if !safe_key(m.name.trim_end_matches('/')) {
            return Err(Reject::Path);
        }
        let fmt = (m.external_attr >> 16) & S_IFMT;
        if !(fmt == 0 || fmt == S_IFREG || fmt == S_IFDIR) || m.flags & zip::FLAG_ENCRYPTED != 0 {
            return Err(Reject::EntryType);
        }
        if u64::from(m.usize) > MAX_FILE
            || (m.method != zip::METHOD_STORED && m.method != zip::METHOD_DEFLATED)
        {
            return Err(Reject::EntryLimits);
        }
        let is_dir = m.name.ends_with('/');
        if is_dir && m.usize != 0 {
            return Err(Reject::DirectoryNotEmpty);
        }
        if (fmt == S_IFDIR && !is_dir) || (fmt == S_IFREG && is_dir) {
            return Err(Reject::TypeMismatch);
        }
        total += u64::from(m.usize);
    }
    if total > MAX_EXPANDED {
        return Err(Reject::Expanded);
    }
    // Stricter: the whole container is checked before any member is read.
    let located = zip::locate_all(body, &members, cd_offset)?;
    let index: HashMap<&str, usize> = members
        .iter()
        .enumerate()
        .map(|(i, m)| (m.name.as_str(), i))
        .collect();
    let read = |name: &str| -> Result<Vec<u8>, Reject> {
        let i = *index.get(name).ok_or(Reject::Invalid)?;
        let (m, loc) = members.get(i).zip(located.get(i)).ok_or(Reject::Invalid)?;
        zip::read(body, loc, m)
    };
    let manifest_member = index
        .get("manifest.json")
        .and_then(|&i| members.get(i))
        .ok_or(Reject::Invalid)?;
    if u64::from(manifest_member.usize) > MAX_MANIFEST {
        return Err(Reject::ManifestSize);
    }
    let text = json_text(&read("manifest.json")?).ok_or(Reject::Invalid)?;
    let manifest: Value = serde_json::from_str(&text).map_err(|_| Reject::Invalid)?;
    let manifest = manifest.as_object().ok_or(Reject::ManifestObject)?;
    if manifest.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err(Reject::Format);
    }
    let (rows, dir_rows) = match (
        manifest.get("files").and_then(Value::as_array),
        manifest.get("directories").and_then(Value::as_array),
    ) {
        (Some(r), Some(d)) if r.len() <= MAX_RECORDS && d.len() <= MAX_RECORDS => (r, d),
        _ => return Err(Reject::Counts),
    };
    let mut dirs: HashSet<&str> = HashSet::new();
    for d in dir_rows {
        let d = d.as_str().filter(|d| safe_key(d)).ok_or(Reject::Path)?;
        if !dirs.insert(d) {
            return Err(Reject::DuplicateDirectory);
        }
    }
    let mut files: Vec<(VerifiedFile, Vec<u8>)> = Vec::new();
    let mut file_set: HashSet<&str> = HashSet::new();
    for row in rows {
        let row = row.as_object().ok_or(Reject::FileRecord)?;
        let bytes = int(row.get("bytes")).ok_or(Reject::FileRecord)?;
        let sha = row.get("sha256").and_then(Value::as_str);
        let (bytes, sha) = match (u64::try_from(bytes), sha) {
            (Ok(b), Some(s)) => (b, s),
            _ => return Err(Reject::FileRecord),
        };
        let path = row
            .get("path")
            .ok_or(Reject::Invalid)?
            .as_str()
            .filter(|p| safe_key(p))
            .ok_or(Reject::Path)?;
        if file_set.contains(path) || dirs.contains(path) {
            return Err(Reject::Overlap);
        }
        let data = read(&format!("workspace/{path}"))?;
        let digest = sha256_hex(&data);
        if data.len() as u64 != bytes || digest != sha {
            return Err(Reject::Checksum);
        }
        file_set.insert(path);
        files.push((
            VerifiedFile {
                path: path.to_owned(),
                bytes,
                sha256: digest,
            },
            if retain { data } else { Vec::new() },
        ));
    }
    for path in file_set.iter().chain(dirs.iter()) {
        let mut parent = *path;
        while let Some(cut) = parent.rfind('/') {
            parent = parent.get(..cut).unwrap_or_default();
            if file_set.contains(parent) {
                return Err(Reject::ParentOverlap);
            }
        }
    }
    let mut expected: HashSet<String> = HashSet::new();
    expected.insert("manifest.json".into());
    expected.insert("workspace/".into());
    expected.extend(file_set.iter().map(|p| format!("workspace/{p}")));
    expected.extend(dirs.iter().map(|d| format!("workspace/{d}/")));
    if expected.len() != names.len() || !names.iter().all(|n| expected.contains(*n)) {
        return Err(Reject::EntriesMismatch);
    }
    let total: u64 = files.iter().map(|(f, _)| f.bytes).sum();
    let (count, declared) = match (int(manifest.get("fileCount")), int(manifest.get("bytes"))) {
        (Some(c), Some(b)) => (c, b),
        _ => return Err(Reject::TotalsInvalid),
    };
    if total > MAX_TOTAL || count != files.len() as i128 || declared != i128::from(total) {
        return Err(Reject::TotalsMismatch);
    }
    files.sort_by(|(a, _), (b, _)| a.path.cmp(&b.path));
    let (files, contents) = files.into_iter().unzip();
    let mut directories: Vec<String> = dirs.into_iter().map(str::to_owned).collect();
    directories.sort();
    Ok(VerifiedArchive {
        listing: Verified {
            files,
            directories,
            bytes: total,
        },
        settings: if retain {
            manifest
                .get("settings")
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default()))
        } else {
            Value::Null
        },
        fingerprint: if retain {
            sha256_hex(body)
        } else {
            String::new()
        },
        contents,
    })
}
