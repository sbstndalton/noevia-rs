use diary_backup_format::{sha256, tenant_uuid};
use serde::Serialize;
use serde_json::Value;
use std::os::unix::fs::DirBuilderExt;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
type Result<T> = std::result::Result<T, String>;
fn invalid(text: &str) -> String {
    format!("ValueError: {text}")
}
fn io(error: std::io::Error, path: &Path) -> String {
    let kind = match error.kind() {
        std::io::ErrorKind::NotFound => "FileNotFoundError",
        std::io::ErrorKind::AlreadyExists => "FileExistsError",
        std::io::ErrorKind::PermissionDenied => "PermissionError",
        std::io::ErrorKind::IsADirectory => "IsADirectoryError",
        std::io::ErrorKind::NotADirectory => "NotADirectoryError",
        _ => "OSError",
    };
    let message = error.to_string();
    let message = message.split(" (os error").next().unwrap_or(&message);
    format!(
        "{kind}: [Errno {}] {message}: '{}'",
        error.raw_os_error().unwrap_or(0),
        path.display()
    )
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a Value> {
    v.get(key).ok_or_else(|| format!("KeyError: '{key}'"))
}
fn text(v: &Value) -> Result<&str> {
    v.as_str()
        .ok_or_else(|| "TypeError: expected string".into())
}
fn safe_key(v: &Value) -> Result<&str> {
    let s = v.as_str().ok_or_else(|| invalid("Invalid corpus path"))?;
    if s.starts_with('/')
        || s.contains(['\\', '\0'])
        || s.split('/').any(|p| ["", ".", ".."].contains(&p))
    {
        return Err(invalid("Invalid corpus path"));
    }
    Ok(s)
}
// Path.resolve(strict=False): resolve existing symlinks, allowing absent trailing components.
fn resolve(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().map_err(|e| io(e, path))?.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::CurDir => {}
            _ => {
                resolved.push(component);
                if fs::symlink_metadata(&resolved).is_ok() {
                    resolved = fs::canonicalize(&resolved).map_err(|e| io(e, &resolved))?;
                }
            }
        }
    }
    Ok(resolved)
}
fn mkdir(path: &Path, parents: bool, exist_ok: bool) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => Ok(()),
        Err(e) if parents && e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| io(e, parent))?;
            }
            mkdir(path, false, exist_ok)
        }
        Err(_) if exist_ok && path.is_dir() => Ok(()),
        Err(e) => Err(io(e, path)),
    }
}
#[derive(Serialize, Debug)]
pub struct Restored {
    pub files: usize,
    pub bytes: usize,
    pub tenant: String,
    pub settings: Value,
}
pub fn restore(root: &Path, manifest_path: &Path, destination: &Path) -> Result<Restored> {
    let root = resolve(root)?;
    let manifest_path = resolve(manifest_path)?;
    if destination.exists() {
        return Err(invalid(
            "Restore requires a new directory; existing data will not be overwritten",
        ));
    }
    let body = fs::read(&manifest_path).map_err(|e| io(e, &manifest_path))?;
    if body.len() > 8 * 1024 * 1024
        || manifest_path.file_stem().and_then(|s| s.to_str()) != Some(sha256(&body).as_str())
    {
        return Err(invalid(
            "Manifest checksum does not match its immutable filename",
        ));
    }
    let manifest: Value =
        serde_json::from_slice(&body).map_err(|e| format!("JSONDecodeError: {e}"))?;
    if manifest.get("format").and_then(Value::as_str) != Some("noevia-diary-backup-v1") {
        return Err(invalid("Unsupported backup manifest"));
    }
    let tenant = tenant_uuid(text(field(&manifest, "tenant")?)?)
        .map_err(|_| invalid("badly formed hexadecimal UUID string"))?;
    let files = field(&manifest, "files")?
        .as_array()
        .filter(|a| a.len() <= 50000)
        .ok_or_else(|| invalid("Invalid backup file list"))?;
    let mut verified: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total = 0usize;
    for row in files {
        let relative = safe_key(field(row, "path")?)?;
        let object = safe_key(field(row, "object")?)?;
        if !format!("/{object}").contains(&format!("/noevia-backups/{tenant}/objects/")) {
            return Err(invalid("Backup object has the wrong tenant"));
        }
        let source = resolve(&root.join(object))?;
        if source == root
            || !source.starts_with(&root)
            || verified.iter().any(|(p, _)| p == relative)
        {
            return Err(invalid("Backup escaped its root or duplicated a path"));
        }
        let data = fs::read(&source).map_err(|e| io(e, &source))?;
        total = total.saturating_add(data.len());
        if total > 1024 * 1024 * 1024
            || field(row, "sha256")?.as_str() != Some(sha256(&data).as_str())
        {
            return Err(invalid(
                "Backup object checksum mismatch or restore exceeds 1 GiB",
            ));
        }
        verified.push((relative.into(), data));
    }
    let empty = Vec::new();
    let dirs = match manifest.get("directories") {
        None => &empty,
        Some(v) => v
            .as_array()
            .ok_or("TypeError: directories is not iterable")?,
    };
    let directories = dirs.iter().map(safe_key).collect::<Result<Vec<_>>>()?;
    for path in verified
        .iter()
        .map(|(p, _)| p.as_str())
        .chain(directories.iter().copied())
    {
        if Path::new(path)
            .ancestors()
            .skip(1)
            .any(|parent| verified.iter().any(|(p, _)| Path::new(p) == parent))
            || directories
                .iter()
                .any(|d| verified.iter().any(|(p, _)| p == d))
        {
            return Err(invalid("Backup has overlapping file and directory paths"));
        }
    }
    mkdir(destination, false, false)?;
    for directory in directories {
        mkdir(&destination.join(directory), true, true)?;
    }
    for (relative, data) in &verified {
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            mkdir(parent, true, true)?;
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|e| io(e, &target))?;
        output
            .write_all(data)
            .and_then(|_| output.flush())
            .and_then(|_| output.sync_all())
            .map_err(|e| io(e, &target))?;
    }
    Ok(Restored {
        files: verified.len(),
        bytes: total,
        tenant,
        settings: manifest
            .get("settings")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    })
}
/// Python json.dumps defaults to ensure_ascii, including supplementary-plane surrogate pairs.
pub fn pretty(value: &impl Serialize) -> Result<String> {
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    let mut out = String::new();
    for c in json.chars() {
        if c.is_ascii() && c != '\u{7f}' {
            out.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    Ok(out)
}
