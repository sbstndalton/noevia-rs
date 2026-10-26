//! Bounded, filesystem-free authoring of Diary workspace ZIP exports (#1071).
//! See README.md for the versioned stdin framing and refusal contract.
#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

pub const MAGIC: &[u8] = b"NOEVIA-WORKSPACE-EXPORT-1\n";
pub const OUTPUT_MAGIC: &[u8] = b"NOEVIA-WORKSPACE-EXPORT-OK-1\n";
pub const OUTPUT_END: &[u8] = b"NOEVIA-WORKSPACE-EXPORT-END-1\n";
pub const MAX_HEADER: usize = 16 * 1024 * 1024;
pub const MAX_FILE: usize = 64 * 1024 * 1024;
pub const MAX_TOTAL: usize = 256 * 1024 * 1024;
pub const MAX_MANIFEST: usize = 8 * 1024 * 1024;
pub const MAX_ARCHIVE: usize = 272 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 5000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    Frame,
    Counts,
    Path,
    SourceLimits,
    Directories,
    Overlap,
    Manifest,
    Archive,
}
impl Reject {
    pub fn code(self) -> &'static str {
        match self {
            Self::Frame => "frame",
            Self::Counts => "counts",
            Self::Path => "path",
            Self::SourceLimits => "source_limits",
            Self::Directories => "directories",
            Self::Overlap => "overlap",
            Self::Manifest => "manifest",
            Self::Archive => "archive",
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    settings: Box<serde_json::value::RawValue>,
    directories: Vec<String>,
    files: Vec<File>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    path: String,
    bytes: u64,
}

fn safe(path: &str) -> Result<(), Reject> {
    // ZIP's u16 filename length includes "workspace/" and an optional slash.
    if path.len() > 65524
        || path.starts_with('/')
        || path.contains(['\\', '\0'])
        || path.split('/').any(|part| matches!(part, "" | "." | ".."))
    {
        return Err(Reject::Path);
    }
    Ok(())
}
fn parents(path: &str, dirs: &mut BTreeSet<String>) {
    for (position, _) in path.match_indices('/') {
        if let Some(parent) = path.get(..position) {
            dirs.insert(parent.to_owned());
        }
    }
}

/// Read precisely one frame, validate the full snapshot and assemble the entire ZIP.
/// No caller-visible output occurs until this returns Ok.
pub fn export(mut input: impl Read) -> Result<Vec<u8>, Reject> {
    let mut magic = vec![0; MAGIC.len()];
    input.read_exact(&mut magic).map_err(|_| Reject::Frame)?;
    if magic != MAGIC {
        return Err(Reject::Frame);
    }
    let mut length = [0; 4];
    input.read_exact(&mut length).map_err(|_| Reject::Frame)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_HEADER {
        return Err(Reject::Frame);
    }
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes).map_err(|_| Reject::Frame)?;
    let header: Header = serde_json::from_slice(&bytes).map_err(|_| Reject::Frame)?;
    if header.files.len() > MAX_ENTRIES {
        return Err(Reject::Counts);
    }
    if header.directories.len() > MAX_ENTRIES {
        return Err(Reject::Directories);
    }
    let mut total = 0usize;
    let mut files = BTreeMap::new();
    let mut dirs = BTreeSet::new();
    for dir in header.directories {
        safe(&dir)?;
        parents(&dir, &mut dirs);
        dirs.insert(dir);
        if dirs.len() > MAX_ENTRIES {
            return Err(Reject::Directories);
        }
    }
    // Validate all declarations before allocating any source body.
    for file in &header.files {
        safe(&file.path)?;
        let size = usize::try_from(file.bytes).map_err(|_| Reject::SourceLimits)?;
        if size > MAX_FILE {
            return Err(Reject::SourceLimits);
        }
        total = total
            .checked_add(size)
            .filter(|n| *n <= MAX_TOTAL)
            .ok_or(Reject::SourceLimits)?;
        if files.insert(file.path.clone(), Vec::new()).is_some() {
            return Err(Reject::Overlap);
        }
        parents(&file.path, &mut dirs);
        if dirs.len() > MAX_ENTRIES {
            return Err(Reject::Directories);
        }
    }
    if dirs.iter().any(|dir| files.contains_key(dir)) {
        return Err(Reject::Overlap);
    }
    for file in header.files {
        let size = usize::try_from(file.bytes).map_err(|_| Reject::SourceLimits)?;
        let mut data = vec![0; size];
        input.read_exact(&mut data).map_err(|_| Reject::Frame)?;
        files.insert(file.path, data);
    }
    let mut extra = [0; 1];
    if input.read(&mut extra).map_err(|_| Reject::Frame)? != 0 {
        return Err(Reject::Frame);
    }
    let records: Vec<Value> = files.iter().map(|(path, data)| {
        json!({"path": path, "bytes": data.len(), "sha256": format!("{:x}", Sha256::digest(data))})
    }).collect();
    #[derive(Serialize)]
    struct Manifest<'a> {
        format: &'static str,
        settings: &'a serde_json::value::RawValue,
        directories: &'a BTreeSet<String>,
        #[serde(rename = "fileCount")]
        file_count: usize,
        bytes: usize,
        files: Vec<Value>,
    }
    let manifest = Manifest {
        format: "noevia-workspace-v1",
        settings: &header.settings,
        directories: &dirs,
        file_count: files.len(),
        bytes: total,
        files: records,
    };
    let manifest = serde_json::to_vec_pretty(&manifest).map_err(|_| Reject::Manifest)?;
    if manifest.len() > MAX_MANIFEST {
        return Err(Reject::Manifest);
    }
    let mut zip = Zip::default();
    zip.entry("manifest.json", &manifest, false)?;
    zip.entry("workspace/", &[], true)?;
    for dir in dirs {
        zip.entry(&format!("workspace/{dir}/"), &[], true)?;
    }
    for (path, data) in files {
        zip.entry(&format!("workspace/{path}"), &data, false)?;
    }
    zip.finish()
}

#[derive(Default)]
struct Zip {
    bytes: Vec<u8>,
    central: Vec<u8>,
    count: u16,
}
fn word(out: &mut Vec<u8>, value: u16) {
    out.extend(value.to_le_bytes());
}
fn dword(out: &mut Vec<u8>, value: u32) {
    out.extend(value.to_le_bytes());
}
impl Zip {
    fn entry(&mut self, name: &str, data: &[u8], directory: bool) -> Result<(), Reject> {
        let name_length = u16::try_from(name.len()).map_err(|_| Reject::Path)?;
        let size = u32::try_from(data.len()).map_err(|_| Reject::Archive)?;
        let offset = u32::try_from(self.bytes.len()).map_err(|_| Reject::Archive)?;
        // Reserve the complete local/central pair and final EOCD before appending.
        let projected = self
            .bytes
            .len()
            .checked_add(self.central.len())
            .and_then(|n| n.checked_add(30 + 46 + 22 + name.len() * 2))
            .and_then(|n| n.checked_add(data.len()))
            .ok_or(Reject::Archive)?;
        if projected > MAX_ARCHIVE {
            return Err(Reject::Archive);
        }
        let crc = crc32fast::hash(data);
        dword(&mut self.bytes, 0x04034b50);
        for v in [20, 0x800, 0, 0, 33] {
            word(&mut self.bytes, v);
        }
        for v in [crc, size, size] {
            dword(&mut self.bytes, v);
        }
        word(&mut self.bytes, name_length);
        word(&mut self.bytes, 0);
        self.bytes.extend(name.as_bytes());
        self.bytes.extend(data);
        dword(&mut self.central, 0x02014b50);
        for v in [0x0314, 20, 0x800, 0, 0, 33] {
            word(&mut self.central, v);
        }
        for v in [crc, size, size] {
            dword(&mut self.central, v);
        }
        for v in [name_length, 0, 0, 0, 0] {
            word(&mut self.central, v);
        }
        dword(
            &mut self.central,
            if directory {
                (0o40775 << 16) | 0x10
            } else {
                0o600 << 16
            },
        );
        dword(&mut self.central, offset);
        self.central.extend(name.as_bytes());
        self.count = self.count.checked_add(1).ok_or(Reject::Archive)?;
        Ok(())
    }
    fn finish(mut self) -> Result<Vec<u8>, Reject> {
        let size = u32::try_from(self.central.len()).map_err(|_| Reject::Archive)?;
        let offset = u32::try_from(self.bytes.len()).map_err(|_| Reject::Archive)?;
        self.bytes.extend(self.central);
        dword(&mut self.bytes, 0x06054b50);
        for v in [0, 0, self.count, self.count] {
            word(&mut self.bytes, v);
        }
        dword(&mut self.bytes, size);
        dword(&mut self.bytes, offset);
        word(&mut self.bytes, 0);
        if self.bytes.len() > MAX_ARCHIVE {
            return Err(Reject::Archive);
        }
        Ok(self.bytes)
    }
}
