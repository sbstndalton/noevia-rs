use crate::value::{Kind, Value};
use diary_backup_format::{sha256, tenant_uuid};
use std::collections::HashSet;
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
pub(crate) fn io(error: std::io::Error, path: &Path) -> String {
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
fn field(v: &Value, key: &str) -> Result<Value> {
    match v.kind() {
        Kind::Map(_) => v.get(key).ok_or_else(|| format!("KeyError: '{key}'")),
        Kind::Seq(_) => Err("TypeError: list indices must be integers or slices, not str".into()),
        Kind::Str(_) => Err("TypeError: string indices must be integers, not 'str'".into()),
        _ => Err(format!(
            "TypeError: '{}' object is not subscriptable",
            v.type_name()
        )),
    }
}
fn text(v: &Value) -> Result<String> {
    v.as_str()
        .ok_or_else(|| "TypeError: expected string".into())
}
fn safe_key(v: &Value) -> Result<String> {
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
#[derive(Debug)]
pub struct Restored {
    pub files: usize,
    pub bytes: usize,
    pub tenant: String,
    pub settings: Value,
}
pub fn restore(root: &Path, manifest_path: &Path, destination: &Path) -> Result<Restored> {
    let destination = if destination.is_absolute() {
        destination.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|e| io(e, destination))?
            .join(destination)
    };
    let destination = destination.as_path();
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
    let manifest = crate::value::json(&body)?;
    if manifest.entries().is_none() {
        return Err(format!(
            "AttributeError: '{}' object has no attribute 'get'",
            manifest.type_name()
        ));
    }
    if manifest.get("format").and_then(|v| v.as_str()).as_deref() != Some("noevia-diary-backup-v1")
    {
        return Err(invalid("Unsupported backup manifest"));
    }
    let tenant = tenant_uuid(&text(&field(&manifest, "tenant")?)?)
        .map_err(|_| invalid("badly formed hexadecimal UUID string"))?;
    let files = field(&manifest, "files")?
        .as_array()
        .filter(|a| a.len() <= 50000)
        .ok_or_else(|| invalid("Invalid backup file list"))?;
    let mut verified: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total = 0usize;
    let mut verified_paths = HashSet::new();
    for row in files {
        let relative = safe_key(&field(&row, "path")?)?;
        let object = safe_key(&field(&row, "object")?)?;
        if !format!("/{object}").contains(&format!("/noevia-backups/{tenant}/objects/")) {
            return Err(invalid("Backup object has the wrong tenant"));
        }
        let source = resolve(&root.join(&object))?;
        if source == root || !source.starts_with(&root) || verified_paths.contains(&relative) {
            return Err(invalid("Backup escaped its root or duplicated a path"));
        }
        let data = fs::read(&source).map_err(|e| io(e, &source))?;
        total = total.saturating_add(data.len());
        if total > 1024 * 1024 * 1024
            || field(&row, "sha256")?.as_str().as_deref() != Some(sha256(&data).as_str())
        {
            return Err(invalid(
                "Backup object checksum mismatch or restore exceeds 1 GiB",
            ));
        }
        verified_paths.insert(relative.clone());
        verified.push((relative, data));
    }
    let dirs = match manifest.get("directories").map(|v| v.kind()) {
        None => Vec::new(),
        Some(Kind::Seq(v)) => v,
        Some(Kind::Map(v)) => v.into_iter().map(|(k, _)| k).collect(),
        Some(Kind::Str(s)) => s.chars().map(|c| Value::string(c.to_string())).collect(),
        Some(_) => {
            return Err(format!(
                "TypeError: '{}' object is not iterable",
                manifest
                    .get("directories")
                    .unwrap_or_else(Value::null)
                    .type_name()
            ))
        }
    };
    let directories = dirs.iter().map(safe_key).collect::<Result<Vec<_>>>()?;
    // Match Python dictionary membership: index once, then check each ancestor
    // and each directory collision once. No file×directory cross-product.
    for path in verified
        .iter()
        .map(|(p, _)| p.as_str())
        .chain(directories.iter().map(String::as_str))
    {
        if Path::new(path)
            .ancestors()
            .skip(1)
            .any(|parent| parent.to_str().is_some_and(|p| verified_paths.contains(p)))
        {
            return Err(invalid("Backup has overlapping file and directory paths"));
        }
    }
    if directories.iter().any(|path| verified_paths.contains(path)) {
        return Err(invalid("Backup has overlapping file and directory paths"));
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
        settings: manifest.get("settings").unwrap_or_else(Value::map),
    })
}
/// The same insertion order and ensure_ascii/float rules as json.dumps(indent=2).
pub fn pretty(restored: &Restored) -> Result<String> {
    let map = Value::map();
    map.set("files", Value::new(Kind::Int(restored.files.to_string())))?;
    map.set("bytes", Value::new(Kind::Int(restored.bytes.to_string())))?;
    map.set("tenant", Value::string(&restored.tenant))?;
    map.set("settings", restored.settings.clone())?;
    map.pretty()
}
