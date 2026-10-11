//! Fixed readonly-cache copy. Final-fd NOFOLLOW does not prove ancestor stability,
//! registry integrity or installation ancestry. Logical caps do not bound RSS/OOM.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const SOURCE: &str = "/tmp/s1-diagnose-node-dependencies";
const DESTINATION: &str = "/tmp/m6-chatgpt-status-node-dependencies";
const PROOF: &str = "/tmp/s1-diagnose-dependency-copy.json";
const PROOF_HASH: &str = "98e91b95e8812e863a70dce0fe66bf39c4507b7e50b660936d244d509ad819cc";
const MANIFESTS: [(&str, &str); 2] = [
    (
        "package.json",
        "f3f9375884302c7dcdd7fb510b7804f5dadcdced8ccabdc827638378353db19c",
    ),
    (
        "package-lock.json",
        "ad078a36d1f9b72ba7ffeb2fa945deca872ea81742c8fe1ed5c9323ddcf497e9",
    ),
];
fn require(ok: bool, why: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(why.into())
    }
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn push<T>(queue: &mut Vec<T>, value: T, cap: usize) -> Result<()> {
    require(queue.len() < cap, "pre-append inventory cap")?;
    queue.try_reserve(1)?;
    queue.push(value);
    Ok(())
}
fn read(path: &Path, cap: usize) -> Result<Vec<u8>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC)
                .bits() as i32,
        )
        .open(path)?;
    let meta = file.metadata()?;
    require(
        meta.is_file() && meta.nlink() == 1 && meta.len() <= cap as u64,
        "regular singleton file cap",
    )?;
    let mut result = Vec::new();
    file.take((cap + 1) as u64).read_to_end(&mut result)?;
    require(result.len() <= cap, "read cap")?;
    Ok(result)
}
fn relative(name: &str) -> Result<PathBuf> {
    require(name.len() <= 4096, "name cap")?;
    let value = PathBuf::from(name);
    require(
        !value.is_absolute()
            && value
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "relative normal path required",
    )?;
    Ok(value)
}
#[allow(clippy::indexing_slicing)] // serde_json string-key indexing yields Null, never panics.
pub fn copy() -> Result<Value> {
    let source = Path::new(SOURCE);
    let destination = Path::new(DESTINATION);
    require(!destination.exists(), "fresh private destination required")?;
    let proof_bytes = read(Path::new(PROOF), 2 * 1024 * 1024)?;
    require(
        digest(&proof_bytes) == PROOF_HASH,
        "exact accepted proof identity",
    )?;
    let proof: Value = serde_json::from_slice(&proof_bytes)?;
    require(
        proof["destination"] == SOURCE && proof["total_bytes"] == 19_185_089,
        "proof source and total identity",
    )?;
    let rows = proof["files"].as_array().ok_or("proof files")?;
    require(rows.len() == 1831, "exact file count")?;
    let closure = proof["closure"].as_array().ok_or("proof closure")?;
    require(closure.len() <= 128, "package closure cap before queue")?;
    // Fixed attested inventory replaces a dynamic package dependency queue.
    let mut expected = BTreeMap::new();
    for row in rows {
        let name = row["path"].as_str().ok_or("proof filename")?;
        let path = relative(name)?;
        require(
            path.starts_with("node_modules") && expected.len() < 4096,
            "expected inventory cap",
        )?;
        let bytes = row["bytes"].as_u64().ok_or("proof file bytes")?;
        let hash = row["sha256"].as_str().ok_or("proof file hash")?;
        require(
            bytes <= 16 * 1024 * 1024 && hash.len() == 64,
            "proof file cap/hash",
        )?;
        require(
            expected.insert(path, (bytes, hash.to_owned())).is_none(),
            "unique expected path",
        )?;
    }
    for (name, hash) in MANIFESTS {
        let bytes = read(&source.join(name), 1_048_576)?;
        require(digest(&bytes) == hash, "immutable landed core manifests")?;
        expected.insert(PathBuf::from(name), (bytes.len() as u64, hash.into()));
    }
    let mut pending = Vec::new();
    push(&mut pending, source.to_path_buf(), 4096)?;
    let mut files = Vec::new();
    let mut dependency_bytes = 0usize;
    let mut all_bytes = 0usize;
    while let Some(path) = pending.pop() {
        let metadata = fs::symlink_metadata(&path)?;
        require(!metadata.file_type().is_symlink(), "links refused")?;
        if metadata.is_dir() {
            require(
                metadata.permissions().mode() & 0o777 == 0o700,
                "source private directory mode",
            )?;
            for entry in fs::read_dir(&path)? {
                push(&mut pending, entry?.path(), 4096)?;
            }
        } else {
            require(
                metadata.is_file()
                    && metadata.nlink() == 1
                    && metadata.permissions().mode() & 0o777 == 0o600,
                "source regular singleton mode",
            )?;
            let name = path.strip_prefix(source)?.to_path_buf();
            let (size, hash) = expected.get(&name).ok_or("unattested source filename")?;
            let data = read(&path, 16 * 1024 * 1024)?;
            require(
                data.len() as u64 == *size && digest(&data) == *hash,
                "complete source attestation",
            )?;
            all_bytes = all_bytes.checked_add(data.len()).ok_or("total overflow")?;
            require(all_bytes <= 32 * 1024 * 1024, "total cap before append")?;
            if name.starts_with("node_modules") {
                dependency_bytes = dependency_bytes
                    .checked_add(data.len())
                    .ok_or("dependency total overflow")?;
            }
            push(&mut files, (name, data), 4096)?;
        }
    }
    require(
        files.len() == 1833 && dependency_bytes == 19_185_089 && files.len() == expected.len(),
        "complete attested source identity BEFORE creation",
    )?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    fs::create_dir(destination)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
    let mut inventory = Vec::new();
    for (name, bytes) in files {
        let target = destination.join(&name);
        let parent = target.parent().ok_or("parent")?;
        fs::create_dir_all(parent)?;
        let mut current = Some(parent);
        while let Some(dir) = current {
            require(dir.starts_with(destination), "owned parent containment")?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            if dir == destination {
                break;
            }
            current = dir.parent();
        }
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC)
                    .bits() as i32,
            )
            .open(&target)?;
        output.write_all(&bytes)?;
        require(read(&target, 16 * 1024 * 1024)? == bytes, "copied identity")?;
        push(
            &mut inventory,
            json!({"path":name,"bytes":bytes.len(),"sha256":digest(&bytes),"mode":"100600"}),
            4096,
        )?;
    }
    Ok(
        json!({"source":SOURCE,"destination":DESTINATION,"proof_sha256":PROOF_HASH,"files":inventory,"dependency_files":1831,"dependency_bytes":dependency_bytes,"all_bytes":all_bytes,"root_mode":"0700","installed_or_downloaded":false,"limits":"logical caps; stable trusted ancestors assumed; no registry integrity or ancestry-race proof"}),
    )
}
/// Read-only complete content/mode check of this one owned private destination.
#[allow(clippy::indexing_slicing)] // Only serde_json string-key indexing.
pub fn verify(proof: &Value) -> Result<Value> {
    require(
        proof["source"] == SOURCE && proof["destination"] == DESTINATION,
        "fixed copy proof paths",
    )?;
    let rows = proof["files"].as_array().ok_or("copy rows")?;
    require(rows.len() == 1833, "full copy inventory count")?;
    let mut expected = BTreeMap::new();
    for row in rows {
        let name = relative(row["path"].as_str().ok_or("copy row name")?)?;
        require(
            expected.len() < 4096 && row["mode"] == "100600",
            "bounded fixed copy mode",
        )?;
        let size = row["bytes"].as_u64().ok_or("copy bytes")?;
        let hash = row["sha256"].as_str().ok_or("copy hash")?;
        require(size <= 16 * 1024 * 1024, "copy file cap")?;
        require(
            expected.insert(name, (size, hash.to_owned())).is_none(),
            "unique copy row",
        )?;
    }
    let root = Path::new(DESTINATION);
    require(
        fs::canonicalize(root)? == Path::new("/private/tmp/m6-chatgpt-status-node-dependencies"),
        "one exact canonical tmp alias",
    )?;
    let mut pending = Vec::new();
    push(&mut pending, root.to_path_buf(), 4096)?;
    let (mut visited, mut files, mut total) = (0usize, 0usize, 0usize);
    while let Some(path) = pending.pop() {
        visited += 1;
        require(visited <= 8192, "complete traversal cap")?;
        let metadata = fs::symlink_metadata(&path)?;
        require(
            !metadata.file_type().is_symlink(),
            "private copy links refused",
        )?;
        if metadata.is_dir() {
            require(
                metadata.permissions().mode() & 0o777 == 0o700,
                "complete directory mode",
            )?;
            for entry in fs::read_dir(&path)? {
                push(&mut pending, entry?.path(), 4096)?;
            }
        } else {
            require(
                metadata.is_file()
                    && metadata.nlink() == 1
                    && metadata.permissions().mode() & 0o777 == 0o600,
                "complete copied file mode/class",
            )?;
            let name = path.strip_prefix(root)?;
            let (size, sha) = expected.get(name).ok_or("unexpected copied file")?;
            let bytes = read(&path, 16 * 1024 * 1024)?;
            require(
                bytes.len() as u64 == *size && digest(&bytes) == *sha,
                "complete copied content identity",
            )?;
            total = total
                .checked_add(bytes.len())
                .ok_or("copied total overflow")?;
            files += 1;
            require(
                total <= 32 * 1024 * 1024 && files <= 4096,
                "copied total/files cap",
            )?;
        }
    }
    require(
        files == expected.len() && proof["all_bytes"] == total,
        "complete copy verified",
    )?;
    Ok(
        json!({"files":files,"bytes":total,"canonical_path":"/private/tmp/m6-chatgpt-status-node-dependencies","all_files_mode":"100600","all_directories_mode":"0700","qualification":"stable trusted ancestors; final descriptor and recorded content identity; no registry integrity or ancestry race proof"}),
    )
}

/// Classify every attested file by complete npm installation boundaries. A
/// descendant descriptor is metadata only after all containing roots are known.
fn installed_owner<T>(
    name: &Path,
    lock: &Value,
    inventory: &BTreeMap<PathBuf, T>,
) -> Result<PathBuf> {
    let text = name.to_str().ok_or("dependency path utf8")?;
    let mut parts = text.split('/').peekable();
    require(parts.next() == Some("node_modules"), "installed namespace")?;
    let mut root = PathBuf::from("node_modules");
    loop {
        let first = parts.next().ok_or("installed package component")?;
        require(
            !first.is_empty() && first != "." && first != ".." && first != "node_modules",
            "installed package grammar",
        )?;
        root.push(first);
        if first.starts_with('@') {
            require(first.len() > 1, "installed scope grammar")?;
            let second = parts.next().ok_or("scoped package component")?;
            require(
                !second.is_empty()
                    && !second.starts_with('@')
                    && !matches!(second, "." | ".." | "node_modules"),
                "scoped package grammar",
            )?;
            root.push(second);
        }
        let key = root.to_str().ok_or("installed root utf8")?.to_owned();
        require(
            lock.get("packages")
                .and_then(|p| p.get(&key))
                .is_some_and(Value::is_object)
                && inventory.contains_key(&root.join("package.json")),
            "known installed containing root",
        )?;
        let mut nested = false;
        for part in parts.by_ref() {
            require(
                !part.is_empty() && part != "." && part != "..",
                "dependency descendant grammar",
            )?;
            if part == "node_modules" {
                root.push(part);
                nested = true;
                break;
            }
            // Retain the complete path to any later nested installation.
            root.push(part);
        }
        if !nested {
            // Remove ordinary descendants to recover this installation owner.
            return Ok(PathBuf::from(key));
        }
    }
}
fn validate_installed_manifest(directory: &Path, data: &[u8], lock: &Value) -> Result<()> {
    let package: Value = serde_json::from_slice(data)?;
    let key = directory.to_str().ok_or("package path utf8")?;
    let locked = lock
        .get("packages")
        .and_then(|p| p.get(key))
        .ok_or("installed lock entry")?;
    require(
        locked.is_object()
            && package.get("version").is_some_and(Value::is_string)
            && package.get("version") == locked.get("version"),
        "installed package version in immutable lock",
    )?;
    let installed_name = package
        .get("name")
        .and_then(Value::as_str)
        .ok_or("installed package name")?;
    require(
        key.rsplit_once("node_modules/")
            .is_some_and(|(_, suffix)| suffix == installed_name),
        "installed package path identity",
    )?;
    Ok(())
}

/// Qualification input paths only; never production configuration. Inventories
/// establish current byte identity, not registry integrity or installation ancestry.
pub fn materialize(args: &[String]) -> Result<Value> {
    materialize_with_runtime(args, None, None)
}
// Private sibling-only Linux orchestration supplies its freshly captured metadata
// and evidence-retaining invoker. Neither is accepted from command-line JSON.
type EvidenceInvoker<'a> =
    dyn Fn(&Path, &[&str], &Path, std::time::Duration) -> Result<Vec<u8>> + 'a;
#[allow(clippy::indexing_slicing)] // Same bounded serde_json indexing as the original materializer.
pub(super) fn materialize_with_runtime(
    args: &[String],
    verified_runtime: Option<&[u8]>,
    evidence: Option<&EvidenceInvoker<'_>>,
) -> Result<Value> {
    use std::time::{Duration, Instant};
    require(args.len() == 10, "exact five qualification path pairs")?;
    let names = [
        "--core-checkout",
        "--dependency-root",
        "--dependency-inventory",
        "--node",
        "--owned-output",
    ];
    let mut paths = Vec::new();
    for ([option, value], name) in args.as_chunks::<2>().0.iter().zip(names) {
        require(
            option == name && value.len() <= 4096,
            "fixed bounded path option",
        )?;
        require(
            value
                .split('/')
                .skip(1)
                .all(|part| !part.is_empty() && part != "." && part != ".."),
            "unnormalized input path refused",
        )?;
        let path = PathBuf::from(value);
        require(
            path.is_absolute()
                && path
                    .components()
                    .all(|c| matches!(c, Component::RootDir | Component::Normal(_))),
            "absolute normal trusted path",
        )?;
        push(&mut paths, path, 5)?;
    }
    let [core, deps, inventory, node, out]: [PathBuf; 5] =
        paths.try_into().map_err(|_| "exact path count")?;
    let (core, deps, inventory, node, out) = (&core, &deps, &inventory, &node, &out);
    require(!out.exists(), "fresh owned topology required")?;
    let deadline =
        Instant::now() + Duration::from_secs(if verified_runtime.is_some() { 45 } else { 60 });
    let invoke = |program: &Path, argv: &[&str]| -> Result<Vec<u8>> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or("materialization deadline")?;
        if let Some(evidence) = evidence {
            return evidence(program, argv, core, remaining.min(Duration::from_secs(15)));
        }
        let mut command = std::process::Command::new(program);
        command
            .args(argv)
            .current_dir(core)
            .env_clear()
            .env("PATH", "/usr/bin:/bin");
        let result =
            crate::process::bounded(command, vec![], remaining.min(Duration::from_secs(15)))?;
        require(
            result.status.success() && result.stderr.is_empty(),
            "bounded source command success and empty stderr",
        )?;
        Ok(result.stdout)
    };
    let git = Path::new("/usr/bin/git");
    require(
        invoke(git, &["rev-parse", "HEAD"])? == format!("{}\n", crate::CORE_SHA).as_bytes(),
        "immutable core commit",
    )?;
    require(
        invoke(git, &["rev-parse", "HEAD^{tree}"])?
            == b"345adfa12da083eb5888ec232dc2d86b9b0383a5\n",
        "immutable core tree",
    )?;
    let tree = invoke(
        git,
        &[
            "ls-tree",
            "-rz",
            "--full-tree",
            "HEAD",
            "--",
            "server",
            "package.json",
            "package-lock.json",
        ],
    )?;
    require(tree.len() <= 256 * 1024, "tracked source list cap")?;
    let mut source = Vec::new();
    let mut source_bytes = 0usize;
    for record in tree.split(|c| *c == 0).filter(|s| !s.is_empty()) {
        require(Instant::now() < deadline, "source scan deadline")?;
        let record = std::str::from_utf8(record)?;
        let (identity, name) = record.split_once('\t').ok_or("git record")?;
        if name != "server/package.json"
            && name != "server/package-lock.json"
            && !name.ends_with(".cjs")
        {
            continue;
        }
        let path = relative(name)?;
        require(
            path.starts_with("server") || MANIFESTS.iter().any(|(p, _)| *p == name),
            "fixed source namespace",
        )?;
        require(source.len() < 1024, "source count before read")?;
        let fields: Vec<_> = identity.split(' ').collect();
        let [mode, kind, blob] = fields.as_slice() else {
            return Err("exact git fields".into());
        };
        require(
            matches!(*mode, "100644" | "100755") && *kind == "blob" && blob.len() == 40,
            "regular git blob",
        )?;
        let bytes = invoke(git, &["cat-file", "blob", blob])?;
        source_bytes = source_bytes
            .checked_add(bytes.len())
            .ok_or("source total overflow")?;
        require(
            source_bytes <= 16 * 1024 * 1024,
            "source total before append",
        )?;
        for (manifest, sha) in MANIFESTS {
            if name == format!("server/{manifest}") {
                require(digest(&bytes) == sha, "fixed core manifest")?;
            }
        }
        push(
            &mut source,
            (path, bytes, (*mode).to_owned(), (*blob).to_owned()),
            1024,
        )?;
    }
    require(
        source
            .iter()
            .filter(|(p, _, _, _)| {
                MANIFESTS
                    .iter()
                    .any(|(m, _)| p == Path::new(&format!("server/{m}")))
            })
            .count()
            == 2,
        "both fixed manifests present",
    )?;
    let lock_bytes = &source
        .iter()
        .find(|(p, _, _, _)| p == Path::new("server/package-lock.json"))
        .ok_or("lock source")?
        .1;
    let lock: Value = serde_json::from_slice(lock_bytes)?;
    let supplied = read(inventory, 2 * 1024 * 1024)?;
    let proof: Value = serde_json::from_slice(&supplied)?;
    let rows = proof["files"].as_array().ok_or("supplied file inventory")?;
    require(!rows.is_empty() && rows.len() <= 4096, "inventory row cap")?;
    let mut expected = BTreeMap::new();
    for row in rows {
        let path = relative(row["path"].as_str().ok_or("inventory path")?)?;
        // Root manifests may occur in the historical copy proof but are not deps.
        if MANIFESTS.iter().any(|(name, _)| path == Path::new(name)) {
            continue;
        }
        require(
            path.starts_with("node_modules") && expected.len() < 4096,
            "dependency namespace and preinsert cap",
        )?;
        let size = row["bytes"].as_u64().ok_or("inventory bytes")?;
        let sha = row["sha256"].as_str().ok_or("inventory hash")?;
        let mode = row["mode"].as_str().ok_or("inventory mode")?;
        require(
            size <= 16 * 1024 * 1024
                && sha.len() == 64
                && matches!(mode, "100600" | "100644" | "100755"),
            "dependency file bounds",
        )?;
        require(
            expected
                .insert(path, (size, sha.to_owned(), mode.to_owned()))
                .is_none(),
            "unique supplied file",
        )?;
    }
    let mut queue = Vec::new();
    push(&mut queue, deps.join("node_modules"), 4096)?;
    let mut copied = Vec::new();
    let (mut total, mut visited) = (0usize, 0usize);
    while let Some(path) = queue.pop() {
        require(Instant::now() < deadline, "dependency scan deadline")?;
        visited += 1;
        require(visited <= 8192, "traversal cap")?;
        let meta = fs::symlink_metadata(&path)?;
        require(!meta.file_type().is_symlink(), "dependency links refused")?;
        if meta.is_dir() {
            for entry in fs::read_dir(&path)? {
                push(&mut queue, entry?.path(), 4096)?;
            }
        } else {
            let name = path.strip_prefix(deps)?.to_path_buf();
            let (size, sha, mode) = expected.get(&name).ok_or("unlisted dependency file")?;
            require(
                meta.is_file()
                    && meta.nlink() == 1
                    && format!("100{:03o}", meta.permissions().mode() & 0o777) == *mode,
                "dependency recorded mode/class",
            )?;
            let data = read(&path, 16 * 1024 * 1024)?;
            require(
                data.len() as u64 == *size && digest(&data) == *sha,
                "complete dependency byte identity",
            )?;
            let owner = installed_owner(&name, &lock, &expected)?;
            if name == owner.join("package.json") {
                validate_installed_manifest(&owner, &data, &lock)?;
            }
            total = total.checked_add(data.len()).ok_or("dependency overflow")?;
            require(total <= 64 * 1024 * 1024, "dependency total before append")?;
            push(&mut copied, (name, data, mode.clone()), 4096)?;
        }
    }
    require(
        copied.len() == expected.len(),
        "complete dependency inventory before create",
    )?;
    let node_bytes = read(node, 128 * 1024 * 1024)?;
    let runtime = if let Some(runtime) = verified_runtime {
        runtime.to_vec()
    } else {
        invoke(node, &["-p", "JSON.stringify({version:process.version,abi:process.versions.modules,platform:process.platform,arch:process.arch})"])?
    };
    require(runtime.len() <= 1024, "runtime metadata cap")?;
    let runtime: Value = serde_json::from_slice(&runtime)?;
    require(
        runtime["version"] == "v22.23.3"
            && runtime["abi"] == "127"
            && matches!(runtime["platform"].as_str(), Some("darwin" | "linux")),
        "fixed runtime dialect",
    )?;
    let fixture = out.join("closed-fixture");
    let input = serde_json::to_vec(&crate::fixed_input(
        out.to_str().ok_or("output utf8")?,
        fixture.to_str().ok_or("fixture utf8")?,
    ))?;
    require(
        input.len() <= 8192,
        "fixed seven-case input before creation",
    )?;
    // No oracle invocation here. This pre-execution manifest is the authority for
    // a future fixed adapter capture; every require.cache path must match it.
    fs::create_dir(out)?;
    fs::set_permissions(out, fs::Permissions::from_mode(0o700))?;
    let mut entries = Vec::new();
    for (name, bytes, mode, blob) in source
        .into_iter()
        .map(|(p, b, m, g)| (p, b, m, Some(g)))
        .chain(copied.into_iter().map(|(p, b, m)| (p, b, m, None)))
    {
        require(Instant::now() < deadline, "creation cooperative deadline")?;
        let target = out.join(&name);
        let parent = target.parent().ok_or("owned parent")?;
        fs::create_dir_all(parent)?;
        let mut dir = parent;
        loop {
            require(dir.starts_with(out), "owned containment")?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            if dir == out {
                break;
            }
            dir = dir.parent().ok_or("owned ancestry")?;
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC)
                    .bits() as i32,
            )
            .open(&target)?;
        file.write_all(&bytes)?;
        require(
            read(&target, 16 * 1024 * 1024)? == bytes,
            "materialized file identity",
        )?;
        push(
            &mut entries,
            json!({"path":name,"bytes":bytes.len(),"sha256":digest(&bytes),"source_mode":mode,"git_blob":blob,"destination_mode":"100600"}),
            5120,
        )?;
    }
    entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    fs::create_dir(&fixture)?;
    fs::set_permissions(&fixture, fs::Permissions::from_mode(0o700))?;
    let save = |name: &Path, bytes: &[u8]| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(
                (rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC)
                    .bits() as i32,
            )
            .open(name)?;
        file.write_all(bytes)?;
        Ok(())
    };
    save(&fixture.join("secrets.key"), &[7; 32])?;
    save(&out.join("qualification-input.json"), &input)?;
    save(
        &out.join("qualification-adapter.cjs"),
        crate::ADAPTER.as_bytes(),
    )?;
    let manifest = json!({"core_commit":crate::CORE_SHA,"core_tree":"345adfa12da083eb5888ec232dc2d86b9b0383a5","core_source":core,"dependency_source":deps,"inventory_sha256":digest(&supplied),"node":node,"node_sha256":digest(&node_bytes),"runtime":runtime,"output":out,"canonical_output":fs::canonicalize(out)?,"files":entries,"adapter_sha256":digest(crate::ADAPTER.as_bytes()),"input_sha256":digest(&input),"oracle_executed":false,"limits":"current supplied inventory identity only; no registry integrity, ancestor race, runtime dylib, hard RSS/OOM or kernel deadline proof; missing non-cjs core dependencies fail closed"});
    let encoded = serde_json::to_vec_pretty(&manifest)?;
    require(
        encoded.len() <= 2 * 1024 * 1024,
        "manifest serialization cap",
    )?;
    save(&out.join("qualification-manifest.json"), &encoded)?;
    Ok(manifest)
}
#[cfg(test)]
mod tests {
    #[test]
    fn preappend_queue_cap() -> super::Result<()> {
        let mut values = Vec::new();
        for i in 0..4096 {
            super::push(&mut values, i, 4096)?;
        }
        assert!(super::push(&mut values, 4096, 4096).is_err());
        assert_eq!(values.len(), 4096);
        Ok(())
    }
    #[test]
    fn materialization_rejects_nonfixed_path_grammar_before_io() -> super::Result<()> {
        assert!(super::materialize(&[]).is_err());
        for invalid in ["relative", "/tmp/../other", "/tmp/./other"] {
            let mut args: Vec<String> = [
                "--core-checkout",
                invalid,
                "--dependency-root",
                "/not-used",
                "--dependency-inventory",
                "/not-used",
                "--node",
                "/not-used",
                "--owned-output",
                "/not-used",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect();
            assert!(
                super::materialize(&args).is_err(),
                "M6_MATERIALIZE_ASSERT/path-grammar"
            );
            *args
                .first_mut()
                .ok_or("fixed argument fixture must have first item")? = "--script".into();
            assert!(
                super::materialize(&args).is_err(),
                "M6_MATERIALIZE_ASSERT/fixed-options"
            );
        }
        Ok(())
    }
    #[test]
    fn installed_roots_and_descendant_metadata_fail_closed() -> super::Result<()> {
        use std::{
            collections::BTreeMap,
            path::{Path, PathBuf},
        };
        let lock = serde_json::json!({"packages": {
            "node_modules/plain": {"version":"1"},
            "node_modules/@scope/pkg": {"version":"2"},
            "node_modules/plain/node_modules/nested": {"version":"3"}
        }});
        let inventory: BTreeMap<PathBuf, ()> = [
            "node_modules/plain/package.json",
            "node_modules/@scope/pkg/package.json",
            "node_modules/plain/node_modules/nested/package.json",
        ]
        .into_iter()
        .map(|p| (PathBuf::from(p), ()))
        .collect();
        for (path, owner, manifest) in [
            (
                "node_modules/plain/package.json",
                "node_modules/plain",
                r#"{"name":"plain","version":"1"}"#,
            ),
            (
                "node_modules/@scope/pkg/package.json",
                "node_modules/@scope/pkg",
                r#"{"name":"@scope/pkg","version":"2"}"#,
            ),
            (
                "node_modules/plain/node_modules/nested/package.json",
                "node_modules/plain/node_modules/nested",
                r#"{"name":"nested","version":"3"}"#,
            ),
        ] {
            assert_eq!(
                super::installed_owner(Path::new(path), &lock, &inventory)?,
                Path::new(owner)
            );
            super::validate_installed_manifest(Path::new(owner), manifest.as_bytes(), &lock)?;
        }
        for path in [
            "node_modules/@scope/pkg/esm/package.json",
            "node_modules/plain/modules/package.json",
            "node_modules/plain/examples/package.json",
        ] {
            let owner = super::installed_owner(Path::new(path), &lock, &inventory)?;
            assert_ne!(Path::new(path), owner.join("package.json"));
        }
        for path in [
            "node_modules/unknown/package.json",
            "node_modules/plain/node_modules/unknown/package.json",
            "node_modules/plain/examples/node_modules/unknown/package.json",
            "node_modules/@scope/package.json",
            "node_modules/plain/node_modules",
            "node_modules/plain/../package.json",
        ] {
            assert!(
                super::installed_owner(Path::new(path), &lock, &inventory).is_err(),
                "M6_PACKAGE_SCOPE_ASSERT/unknown-root-or-grammar"
            );
        }
        let mut missing = inventory.clone();
        missing.remove(Path::new("node_modules/plain/package.json"));
        assert!(super::installed_owner(
            Path::new("node_modules/plain/modules/package.json"),
            &lock,
            &missing
        )
        .is_err());
        for manifest in [
            r#"{"name":"plain","version":"9"}"#,
            r#"{"name":"wrong","version":"1"}"#,
            r#"{"name":"plain"}"#,
        ] {
            assert!(
                super::validate_installed_manifest(
                    Path::new("node_modules/plain"),
                    manifest.as_bytes(),
                    &lock
                )
                .is_err(),
                "M6_PACKAGE_SCOPE_ASSERT/name-or-version"
            );
        }
        Ok(())
    }
}
