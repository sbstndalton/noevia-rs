//! Bounded, filesystem-free authoring of Diary workspace ZIP exports (#1071).
//! See README.md for the versioned stdin framing and refusal contract.
#![forbid(unsafe_code)]
use serde::Deserialize;
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
    // The Python exporter's manifest, byte for byte (noevia#2652):
    // json.dumps(manifest, ensure_ascii=False, sort_keys=True, indent=2).
    let records = files
        .iter()
        .map(|(path, data)| {
            Py::Dict(vec![
                ("path".into(), Py::Text(path.clone())),
                ("bytes".into(), Py::Number(data.len().to_string())),
                (
                    "sha256".into(),
                    Py::Text(format!("{:x}", Sha256::digest(data))),
                ),
            ])
        })
        .collect();
    let manifest = Py::Dict(vec![
        ("format".into(), Py::Text("noevia-workspace-v1".into())),
        (
            "settings".into(),
            Py::read(header.settings.get()).ok_or(Reject::Manifest)?,
        ),
        (
            "directories".into(),
            Py::List(dirs.iter().cloned().map(Py::Text).collect()),
        ),
        ("fileCount".into(), Py::Number(files.len().to_string())),
        ("bytes".into(), Py::Number(total.to_string())),
        ("files".into(), Py::List(records)),
    ]);
    let mut text = String::new();
    manifest.dump(0, &mut text);
    let manifest = text.into_bytes();
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

/// A JSON value as Python's `json` module holds and writes it, for the manifest. A number keeps the
/// text it was read with: the header's numbers were written by Python's `json.dumps`, which writes
/// the same number the same way again (a float's `repr`, an int of any size).
enum Py {
    Literal(&'static str),
    Number(String),
    Text(String),
    List(Vec<Py>),
    Dict(Vec<(String, Py)>),
}
impl Py {
    /// The value of one complete JSON text; `None` when it is not one.
    fn read(text: &str) -> Option<Py> {
        let mut cursor = Cursor { text, at: 0 };
        let value = cursor.value(0)?;
        cursor.space();
        (cursor.at == text.len()).then_some(value)
    }
    /// `json.dumps(self, ensure_ascii=False, sort_keys=True, indent=2)` at nesting `level`: keys in
    /// code point order (the last of a repeated key wins, as `json.loads` keeps it), `,` and `: `
    /// separators, an empty list or object on one line, no final newline.
    fn dump(&self, level: usize, out: &mut String) {
        let line = |out: &mut String, level: usize| {
            out.push('\n');
            out.extend(std::iter::repeat_n("  ", level));
        };
        match self {
            Py::Literal(text) => out.push_str(text),
            Py::Number(text) => out.push_str(text),
            Py::Text(text) => py_string(text, out),
            Py::List(items) if items.is_empty() => out.push_str("[]"),
            Py::List(items) => {
                out.push('[');
                for (n, item) in items.iter().enumerate() {
                    if n > 0 {
                        out.push(',');
                    }
                    line(out, level + 1);
                    item.dump(level + 1, out);
                }
                line(out, level);
                out.push(']');
            }
            Py::Dict(items) if items.is_empty() => out.push_str("{}"),
            Py::Dict(items) => {
                let sorted: BTreeMap<&str, &Py> =
                    items.iter().map(|(k, v)| (k.as_str(), v)).collect();
                out.push('{');
                for (n, (key, value)) in sorted.into_iter().enumerate() {
                    if n > 0 {
                        out.push(',');
                    }
                    line(out, level + 1);
                    py_string(key, out);
                    out.push_str(": ");
                    value.dump(level + 1, out);
                }
                line(out, level);
                out.push('}');
            }
        }
    }
}
/// Python's string encoder with `ensure_ascii=False`: `"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`
/// escaped by name, other controls below U+0020 as `\u00XX`, everything else as it is.
fn py_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}
/// A reader over JSON text serde_json already accepted (the header's `settings`).
struct Cursor<'a> {
    text: &'a str,
    at: usize,
}
impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }
    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }
    fn value(&mut self, depth: usize) -> Option<Py> {
        // serde_json refuses nesting past 128 levels before this runs.
        if depth > 256 {
            return None;
        }
        self.space();
        match self.peek()? {
            b'{' => {
                self.at += 1;
                let mut items = Vec::new();
                self.space();
                if self.peek() == Some(b'}') {
                    self.at += 1;
                    return Some(Py::Dict(items));
                }
                loop {
                    self.space();
                    let key = self.string()?;
                    self.space();
                    (self.peek() == Some(b':')).then_some(())?;
                    self.at += 1;
                    items.push((key, self.value(depth + 1)?));
                    self.space();
                    match self.peek()? {
                        b',' => self.at += 1,
                        b'}' => {
                            self.at += 1;
                            return Some(Py::Dict(items));
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                self.space();
                if self.peek() == Some(b']') {
                    self.at += 1;
                    return Some(Py::List(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.space();
                    match self.peek()? {
                        b',' => self.at += 1,
                        b']' => {
                            self.at += 1;
                            return Some(Py::List(items));
                        }
                        _ => return None,
                    }
                }
            }
            b'"' => self.string().map(Py::Text),
            _ => {
                for word in ["true", "false", "null"] {
                    if self.text.get(self.at..)?.starts_with(word) {
                        self.at += word.len();
                        return Some(Py::Literal(word));
                    }
                }
                let start = self.at;
                while matches!(
                    self.peek(),
                    Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                ) {
                    self.at += 1;
                }
                let number = self.text.get(start..self.at)?;
                serde_json::from_str::<serde::de::IgnoredAny>(number).ok()?;
                Some(Py::Number(number.to_owned()))
            }
        }
    }
    /// One string token, decoded.
    fn string(&mut self) -> Option<String> {
        (self.peek() == Some(b'"')).then_some(())?;
        let start = self.at;
        self.at += 1;
        loop {
            match self.peek()? {
                b'\\' => self.at += 2,
                b'"' => {
                    self.at += 1;
                    break;
                }
                _ => self.at += 1,
            }
        }
        serde_json::from_str(self.text.get(start..self.at)?).ok()
    }
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
