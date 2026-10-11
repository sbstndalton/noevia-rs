//! Qualification-only external locked Node dependencies; immutable source stays clean.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
pub fn manifest(path: &Path) -> Result<Vec<u8>, String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
        .map_err(|_| "dependency manifest open")?;
    let meta = file
        .metadata()
        .map_err(|_| "dependency manifest metadata")?;
    if !meta.is_file() || meta.nlink() != 1 || meta.len() > 1_048_576 {
        return Err("dependency manifest class/size".into());
    }
    let mut bytes = Vec::new();
    file.take(1_048_577)
        .read_to_end(&mut bytes)
        .map_err(|_| "dependency manifest read")?;
    if bytes.len() > 1_048_576 {
        return Err("dependency manifest cap".into());
    }
    Ok(bytes)
}
pub fn verify(
    core: &Path,
    runtime: &Path,
    parameter: Option<&Path>,
    expected: &[Vec<u8>; 2],
) -> Result<(PathBuf, Value), String> {
    let supplied = parameter.ok_or("external prompt dependencies required")?;
    let text = supplied.to_str().ok_or("dependency path UTF8")?;
    if text.len() > 4096 || text.contains(':') || !supplied.is_absolute() {
        return Err("dependency path shape".into());
    }
    let root = supplied
        .canonicalize()
        .map_err(|_| "dependency root missing")?;
    let core = core.canonicalize().map_err(|_| "core root missing")?;
    let runtime = runtime.canonicalize().map_err(|_| "runtime root missing")?;
    if root.starts_with(&core)
        || root.starts_with(&runtime)
        || core.starts_with(&root)
        || runtime.starts_with(&root)
    {
        return Err("dependency root overlaps source".into());
    }
    let metadata = fs::symlink_metadata(&root).map_err(|_| "dependency root metadata")?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err("dependency root must be private owned directory".into());
    }
    let modules = root.join("node_modules");
    let module_metadata = fs::symlink_metadata(&modules).map_err(|_| "node_modules absent")?;
    if !module_metadata.is_dir() {
        return Err("node_modules must be directory".into());
    }
    let canonical = modules
        .canonicalize()
        .map_err(|_| "node_modules canonical")?;
    if canonical != modules
        || canonical
            .to_str()
            .is_none_or(|s| s.len() > 4096 || s.contains(':'))
    {
        return Err("node_modules path shape".into());
    }
    let mut hashes = serde_json::Map::new();
    for (index, name) in ["package.json", "package-lock.json"].iter().enumerate() {
        let bytes = manifest(&root.join(name))?;
        if &bytes != expected.get(index).ok_or("dependency manifest index")? {
            return Err("external dependency manifest differs from immutable core".into());
        }
        hashes.insert(
            (*name).into(),
            json!({"sha256":format!("{:x}",Sha256::digest(&bytes)),"bytes":bytes.len()}),
        );
    }
    Ok((
        canonical,
        json!({"external_root":root,"node_modules":modules,"manifests":hashes,"installation":"workflow npm ci from immutable core manifests in private external root","installed_files_integrity_independently_verified":false,"source_checkout_dependencies_installed":false}),
    ))
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, [Vec<u8>; 2]) {
        let owned = tempfile::tempdir().unwrap();
        let core = owned.path().join("core");
        let runtime = owned.path().join("runtime");
        let deps = owned.path().join("dependencies");
        for root in [&core, &runtime, &deps] {
            fs::create_dir(root).unwrap();
            fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::create_dir(deps.join("node_modules")).unwrap();
        let expected = [b"synthetic package".to_vec(), b"synthetic lock".to_vec()];
        fs::write(deps.join("package.json"), &expected[0]).unwrap();
        fs::write(deps.join("package-lock.json"), &expected[1]).unwrap();
        (owned, core, runtime, deps, expected)
    }
    #[test]
    fn external_dependency_manifests_and_path_bounds() {
        let (_owned, core, runtime, deps, expected) = fixture();
        let (modules, proof) = verify(&core, &runtime, Some(&deps), &expected).unwrap();
        assert_eq!(modules, deps.join("node_modules").canonicalize().unwrap());
        assert_eq!(
            proof["installed_files_integrity_independently_verified"],
            false
        );
        assert!(verify(&core, &runtime, None, &expected).is_err());
        assert!(verify(&core, &runtime, Some(&core), &expected).is_err());
        assert!(verify(&core, &runtime, Some(&runtime), &expected).is_err());
        let child = core.join("dependencies");
        fs::create_dir(&child).unwrap();
        assert!(verify(&core, &runtime, Some(&child), &expected).is_err());
        assert!(verify(&core, &runtime, Some(Path::new("/tmp/a:b")), &expected).is_err());
        fs::write(deps.join("package.json"), "wrong").unwrap();
        assert!(verify(&core, &runtime, Some(&deps), &expected).is_err());
        fs::write(deps.join("package.json"), &expected[0]).unwrap();
        fs::write(deps.join("package-lock.json"), "wrong").unwrap();
        assert!(verify(&core, &runtime, Some(&deps), &expected).is_err());
    }
    #[test]
    fn dependency_manifest_descriptor_rejects_links_classes_and_oversize() {
        let (_owned, core, runtime, deps, expected) = fixture();
        let package = deps.join("package.json");
        fs::remove_file(&package).unwrap();
        std::os::unix::fs::symlink(deps.join("package-lock.json"), &package).unwrap();
        assert!(verify(&core, &runtime, Some(&deps), &expected).is_err());
        fs::remove_file(&package).unwrap();
        fs::hard_link(deps.join("package-lock.json"), &package).unwrap();
        assert!(manifest(&package).is_err());
        fs::remove_file(&package).unwrap();
        fs::create_dir(&package).unwrap();
        assert!(manifest(&package).is_err());
        fs::remove_dir(&package).unwrap();
        let file = fs::File::create(&package).unwrap();
        file.set_len(1_048_577).unwrap();
        assert!(manifest(&package).is_err());
        fs::remove_dir(deps.join("node_modules")).unwrap();
        std::os::unix::fs::symlink(&runtime, deps.join("node_modules")).unwrap();
        assert!(verify(&core, &runtime, Some(&deps), &expected).is_err());
    }
}
