//! noevia#1318: native, read-only `GET /api/v1/download-targets`.
//!
//! Where a download may land: the models folder itself, then each declared
//! (`MODEL_DOWNLOAD_TARGETS`) or mounted folder directly inside it, sorted by name. The Python
//! services route it replaces (`api.download_targets`, which `POST /api/v1/downloads` still uses
//! to validate its `target`, so both sides must agree):
//!
//! ```python
//! root = settings.models_dir
//! out = [{"id": "", "label": "Models folder", "path": str(root)}]
//! names = {n.strip() for n in settings.model_download_targets.split(",") if n.strip()}
//! try:
//!     mounts = services._mount_points()
//!     for entry in os.scandir(root):
//!         if entry.is_dir(follow_symlinks=False) and services._is_mount_point(entry.path, mounts):
//!             names.add(entry.name)
//! except OSError:
//!     pass
//! for name in sorted(names):
//!     path = root / name
//!     if "/" in name or name.startswith(".") or not path.is_dir() or path.is_symlink():
//!         continue
//!     out.append({"id": name, "label": name, "path": str(path)})
//! ```
//!
//! Nothing is written. The scan reads the models folder's top level and the mount table only,
//! both with explicit caps. Deliberate differences (docs/native-download-targets.md): a folder
//! with more than [`ENTRY_CAP`] top-level entries, a mount table over [`MOUNTINFO_CAP`] bytes or
//! more than [`NAME_CAP`] declared names answer 500 instead of being read in full.
use std::{
    collections::{BTreeSet, HashSet},
    ffi::{OsStr, OsString},
    io::Read,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Path, PathBuf},
};

/// Largest number of top-level entries scanned in the models folder.
pub const ENTRY_CAP: usize = 65_536;
/// Largest mount table read (a host with thousands of container mounts is well under this).
pub const MOUNTINFO_CAP: u64 = 4 * 1024 * 1024;
/// Largest number of names `MODEL_DOWNLOAD_TARGETS` may declare.
pub const NAME_CAP: usize = 1024;
/// The mount table Python reads (`services.MOUNTINFO_PATH`).
pub const MOUNTINFO: &str = "/proc/self/mountinfo";

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Json(Vec<u8>),
    /// Python would raise (a name that is not UTF-8), or a native bound was exceeded.
    Failed,
}

pub struct DownloadTargets {
    root: String,
    declared: String,
    mountinfo: PathBuf,
}

impl DownloadTargets {
    /// `root` must be an absolute, normalised `MODELS_DIR` (pathlib's `str(root)` of it is
    /// itself); `declared` is `MODEL_DOWNLOAD_TARGETS` verbatim.
    pub fn new(root: &str, declared: &str, mountinfo: &Path) -> Self {
        Self {
            root: root.to_owned(),
            declared: declared.to_owned(),
            mountinfo: mountinfo.to_owned(),
        }
    }

    pub async fn response(&self) -> Outcome {
        let (root, declared, mountinfo) = (
            self.root.clone(),
            self.declared.clone(),
            self.mountinfo.clone(),
        );
        tokio::task::spawn_blocking(move || targets(&root, &declared, &mountinfo))
            .await
            .unwrap_or(Outcome::Failed)
    }
}

/// True for an absolute path already in pathlib's normal form (what `str(Path(p))` returns).
pub fn normal_root(root: &str) -> bool {
    root.starts_with('/')
        && !root.starts_with("//")
        && (root == "/"
            || (!root.ends_with('/')
                && root
                    .split('/')
                    .skip(1)
                    .all(|segment| !matches!(segment, "" | "." | ".."))))
        && !root.contains('\0')
}

/// Python's `str.isspace` (what `str.strip()` removes): Unicode White_Space plus the
/// information separators U+001C..U+001F.
fn py_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{001c}'..='\u{001f}')
}

/// `{n.strip() for n in value.split(",") if n.strip()}`, or `None` past [`NAME_CAP`].
pub fn declared_names(value: &str) -> Option<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for name in value.split(',').map(|n| n.trim_matches(py_space)) {
        if name.is_empty() {
            continue;
        }
        names.insert(name.to_owned());
        if names.len() > NAME_CAP {
            return None;
        }
    }
    Some(names)
}

/// Python's `str.splitlines()` separators.
fn line_break(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{001c}'
            | '\u{001d}'
            | '\u{001e}'
            | '\u{0085}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

/// `re.sub(r"\\([0-7]{3})", lambda m: chr(int(m.group(1), 8)), field)`: a code point, not a byte.
fn unescape(field: &str) -> String {
    let chars: Vec<char> = field.chars().collect();
    let mut out = String::with_capacity(field.len());
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let digits = chars.get(i + 1..i + 4);
        if c == '\\' {
            if let Some(d) = digits.filter(|d| d.iter().all(|x| ('0'..='7').contains(x))) {
                let value = d.iter().fold(0u32, |n, x| n * 8 + (u32::from(*x) - 48));
                if let Some(decoded) = char::from_u32(value) {
                    out.push(decoded);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `posixpath.normpath` on a str.
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_owned();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else {
        usize::from(path.starts_with('/'))
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part != ".." || (initial == 0 && parts.is_empty()) || parts.last() == Some(&"..") {
            parts.push(part);
        } else if !parts.is_empty() {
            parts.pop();
        }
    }
    let joined = format!("{}{}", "/".repeat(initial), parts.join("/"));
    if joined.is_empty() {
        ".".to_owned()
    } else {
        joined
    }
}

/// `services._mount_points()`; an unreadable table (`None`) is an empty set, which Python's
/// `if not points` treats the same way.
pub fn mount_points(bytes: Option<&[u8]>) -> HashSet<String> {
    let mut points = HashSet::new();
    let Some(bytes) = bytes else {
        return points;
    };
    let text = String::from_utf8_lossy(bytes);
    // splitlines() treats "\r\n" as one break; the empty line between them has < 5 fields anyway.
    for line in text.split(line_break) {
        let fields: Vec<&str> = line.split(' ').collect();
        if let Some(field) = fields.get(4) {
            points.insert(normpath(&unescape(field)));
        }
    }
    points
}

/// `os.path.ismount`: a real (non-symlink) folder on another device than its parent, or the
/// same inode as its parent.
fn ismount(path: &Path) -> bool {
    let Ok(here) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if here.file_type().is_symlink() {
        return false;
    }
    let mut parent = path.as_os_str().to_owned();
    parent.push("/..");
    let Ok(up) = std::fs::symlink_metadata(Path::new(&parent)) else {
        return false;
    };
    here.dev() != up.dev() || here.ino() == up.ino()
}

fn read_mountinfo(path: &Path) -> Result<Option<Vec<u8>>, ()> {
    let Ok(file) = std::fs::File::open(path) else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    match file.take(MOUNTINFO_CAP + 1).read_to_end(&mut bytes) {
        Ok(_) if bytes.len() as u64 > MOUNTINFO_CAP => Err(()),
        Ok(_) => Ok(Some(bytes)),
        Err(_) => Ok(None),
    }
}

fn join(root: &str, name: &OsStr) -> OsString {
    let mut path = OsString::from(root);
    if !root.ends_with('/') {
        path.push("/");
    }
    path.push(name);
    path
}

/// The whole answer, or [`Outcome::Failed`] where Python would raise or a cap is passed.
pub fn targets(root: &str, declared: &str, mountinfo: &Path) -> Outcome {
    let Some(declared) = declared_names(declared) else {
        return Outcome::Failed;
    };
    let mut names: BTreeSet<Vec<u8>> = declared.into_iter().map(String::into_bytes).collect();
    let Ok(table) = read_mountinfo(mountinfo) else {
        return Outcome::Failed;
    };
    let points = mount_points(table.as_deref());
    // Python's `abspath(entry.path) in points or realpath(entry.path) in points`. realpath of a
    // non-symlink entry is realpath(root) joined with its name.
    let real_root = std::fs::canonicalize(root).ok();
    if let Ok(entries) = std::fs::read_dir(root) {
        for (seen, entry) in entries.enumerate() {
            if seen >= ENTRY_CAP {
                return Outcome::Failed;
            }
            // An OSError while iterating ends Python's scan; names found so far stay.
            let Ok(entry) = entry else { break };
            let Ok(kind) = entry.file_type() else { break };
            if !kind.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let path = join(root, &name);
            let found = |candidate: &OsStr| candidate.to_str().is_some_and(|p| points.contains(p));
            let listed = !points.is_empty()
                && (found(&path)
                    || real_root
                        .as_deref()
                        .and_then(Path::to_str)
                        .is_some_and(|real| found(&join(real, &name))));
            if ismount(Path::new(&path)) || listed {
                names.insert(name.into_vec());
            }
        }
    }
    let mut out = String::from("{\"targets\":[");
    out.push_str(&entry_json("", "Models folder", root));
    for name in names {
        if name.contains(&b'/') || name.starts_with(b".") {
            continue;
        }
        let path = join(root, OsStr::from_bytes(&name));
        let target = Path::new(&path);
        if !std::fs::metadata(target).is_ok_and(|m| m.is_dir())
            || std::fs::symlink_metadata(target).is_ok_and(|m| m.file_type().is_symlink())
        {
            continue;
        }
        // FastAPI's UTF-8 encode of a surrogate-escaped name raises: a 500, as there.
        let (Ok(name), Some(path)) = (String::from_utf8(name), path.to_str()) else {
            return Outcome::Failed;
        };
        out.push(',');
        out.push_str(&entry_json(&name, &name, path));
    }
    out.push_str("]}");
    Outcome::Json(out.into_bytes())
}

/// One `{"id","label","path"}` object exactly as `json.dumps(ensure_ascii=False)` writes it.
fn entry_json(id: &str, label: &str, path: &str) -> String {
    let s = |v: &str| serde_json::to_string(v).unwrap_or_default();
    format!(
        "{{\"id\":{},\"label\":{},\"path\":{}}}",
        s(id),
        s(label),
        s(path)
    )
}

#[cfg(test)]
mod tests;
