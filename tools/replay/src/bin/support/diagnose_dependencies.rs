//! Owned, bounded copy of the already cached immutable-core Node dependency closure.
//! This qualification assumes stable trusted ancestor directories during traversal.
//! NOFOLLOW protects the final opened file only; paths are not anchored through an
//! all-component directory-descriptor walk. Metadata/open races are not eliminated.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const ROOTS: [&str; 5] = [
    "@node-rs/argon2",
    "@simplewebauthn/server",
    "better-sqlite3",
    "sqlite-vec",
    "unpdf",
];
fn require(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn push_bounded<T>(pending: &mut Vec<T>, value: T, cap: usize) -> Result<()> {
    require(pending.len() < cap, "dependency pending inventory cap")?;
    pending.try_reserve(1)?;
    pending.push(value);
    Ok(())
}
fn read(path: &Path, cap: u64) -> Result<Vec<u8>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    let meta = file.metadata()?;
    require(
        meta.is_file() && meta.nlink() == 1 && meta.len() <= cap,
        "dependency file class/cap",
    )?;
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes)?;
    require(bytes.len() as u64 <= cap, "dependency read cap")?;
    Ok(bytes)
}
pub fn copy(source: &Path, destination: &Path, expected: &[Vec<u8>; 2]) -> Result<Value> {
    require(
        source == Path::new("/private/tmp/m6-reasoning-node22-deps"),
        "fixed dependency source",
    )?;
    require(
        destination == Path::new("/tmp/s1-diagnose-node-dependencies"),
        "fixed private destination",
    )?;
    require(
        !destination.exists(),
        "fresh dependency destination required",
    )?;
    let mut manifests = Vec::new();
    for (i, name) in ["package.json", "package-lock.json"].iter().enumerate() {
        let bytes = read(&source.join(name), 1_048_576)?;
        require(
            expected.get(i) == Some(&bytes),
            "immutable core manifest mismatch",
        )?;
        manifests.push(bytes);
    }
    let lock: Value = serde_json::from_slice(manifests.get(1).ok_or("lock absent")?)?;
    let packages = lock
        .get("packages")
        .and_then(Value::as_object)
        .ok_or("lock packages absent")?;
    let mut closure = BTreeSet::new();
    let mut queued: BTreeSet<String> = ROOTS.iter().map(|s| (*s).into()).collect();
    let mut pending: Vec<String> = ROOTS.iter().map(|s| (*s).into()).collect();
    while let Some(name) = pending.pop() {
        require(
            name.len() <= 256 && !name.contains("..") && !name.starts_with('/'),
            "package name shape",
        )?;
        if !closure.insert(name.clone()) {
            continue;
        }
        require(closure.len() <= 128, "package closure cap")?;
        let entry = packages
            .get(&format!("node_modules/{name}"))
            .ok_or("package absent from immutable lock")?;
        let dir = source.join("node_modules").join(&name);
        require(
            fs::symlink_metadata(&dir)?.is_dir(),
            "installed package directory required",
        )?;
        let installed: Value =
            serde_json::from_slice(&read(&dir.join("package.json"), 1_048_576)?)?;
        require(
            installed.get("name").and_then(Value::as_str) == Some(name.as_str())
                && installed.get("version") == entry.get("version"),
            "installed package identity differs from lock",
        )?;
        for field in ["dependencies", "optionalDependencies"] {
            if let Some(deps) = entry.get(field).and_then(Value::as_object) {
                for dependency in deps.keys() {
                    let path = source.join("node_modules").join(dependency);
                    if field == "optionalDependencies" && !path.try_exists()? {
                        continue;
                    }
                    if queued.contains(dependency) {
                        continue;
                    }
                    require(queued.len() < 128, "dependency unique package cap")?;
                    push_bounded(&mut pending, dependency.clone(), 128)?;
                    queued.insert(dependency.clone());
                }
            }
        }
    }
    let mut files = Vec::new();
    let mut pending = Vec::new();
    let mut total = 0usize;
    for name in &closure {
        push_bounded(&mut pending, source.join("node_modules").join(name), 4096)?;
    }
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)?;
        require(
            !metadata.file_type().is_symlink(),
            "dependency links refused",
        )?;
        if metadata.is_dir() {
            for entry in fs::read_dir(&path)? {
                push_bounded(&mut pending, entry?.path(), 4096)?;
            }
            require(pending.len() <= 4096, "dependency pending inventory cap")?;
        } else {
            require(metadata.is_file(), "dependency special file refused")?;
            let bytes = read(&path, 16 * 1024 * 1024)?;
            total = total
                .checked_add(bytes.len())
                .ok_or("dependency total overflow")?;
            require(
                total <= 32 * 1024 * 1024 && files.len() < 4096,
                "dependency inventory/total cap",
            )?;
            files.push((path.strip_prefix(source)?.to_path_buf(), bytes));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    fs::create_dir(destination)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
    let mut inventory = Vec::new();
    for (relative, bytes) in &files {
        let target = destination.join(relative);
        let parent = target.parent().ok_or("dependency parent")?;
        fs::create_dir_all(parent)?;
        let mut directory = Some(parent);
        while let Some(current) = directory {
            require(
                current.starts_with(destination),
                "private copy directory escape",
            )?;
            fs::set_permissions(current, fs::Permissions::from_mode(0o700))?;
            if current == destination {
                break;
            }
            directory = current.parent();
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)?;
        file.write_all(bytes)?;
        require(
            read(&target, 16 * 1024 * 1024)? == *bytes,
            "dependency copied identity mismatch",
        )?;
        inventory.push(json!({"path":relative,"bytes":bytes.len(),"sha256":format!("{:x}",Sha256::digest(bytes))}));
    }
    for (i, name) in ["package.json", "package-lock.json"].iter().enumerate() {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(destination.join(name))?;
        file.write_all(manifests.get(i).ok_or("manifest index")?)?;
    }
    Ok(
        json!({"source":source,"destination":destination,"roots":ROOTS,"closure":closure,"files":inventory,"total_bytes":total,"caps":{"files":4096,"packages":128,"total_bytes":33554432,"single_file_bytes":16777216},"install_or_download":false}),
    )
}
#[cfg(test)]
mod tests {
    #[test]
    fn queue_admits_exact_cap_and_refuses_plus_one_before_growth() {
        for cap in [128, 4096] {
            let mut pending = Vec::new();
            for value in 0..cap {
                assert!(super::push_bounded(&mut pending, value, cap).is_ok());
            }
            assert_eq!(pending.len(), cap);
            assert!(super::push_bounded(&mut pending, cap, cap).is_err());
            assert_eq!(pending.len(), cap);
        }
    }
}
