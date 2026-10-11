//! Linux-only private finite factory qualification. Stable trusted ancestors are
//! required; final NOFOLLOW reads do not establish all-component race immunity.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    match value.get(key) {
        Some(value) => value,
        None => &Value::Null,
    }
}
const TREE: &str = "345adfa12da083eb5888ec232dc2d86b9b0383a5";
const NEGATIVE: &str =
    "chatgpt_status_differential::linux_factory_comparator_rejects_grant_timestamp_delta";
const POSITIVE: &str = "chatgpt_status_differential::linux_factory_seven_rows_through_rust_router";
const IDS: [&str; 7] = [
    "live-disconnected",
    "idle-upkeep",
    "unauthorized",
    "device-browser-only",
    "mixed-credential",
    "owner-isolated",
    "product-off-before-schema",
];
const METADATA: &str = "JSON.stringify({version:process.version,abi:process.versions.modules,platform:process.platform,arch:process.arch})";
fn require(ok: bool, why: &'static str) -> Result<()> {
    crate::require(ok, why)
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
        "Linux regular singleton read cap",
    )?;
    let mut bytes = Vec::new();
    file.take((cap + 1) as u64).read_to_end(&mut bytes)?;
    require(bytes.len() <= cap, "Linux complete read cap")?;
    Ok(bytes)
}
fn path(value: &str, existing: bool) -> Result<PathBuf> {
    require(
        value.len() <= 4096
            && value.starts_with('/')
            && value
                .split('/')
                .skip(1)
                .all(|p| !p.is_empty() && p != "." && p != ".."),
        "canonical absolute path grammar",
    )?;
    let path = PathBuf::from(value);
    if existing {
        require(fs::canonicalize(&path)? == path, "canonical existing path")?;
    } else {
        require(
            !path.try_exists()?
                && fs::canonicalize(path.parent().ok_or("output parent")?)?
                    == path.parent().ok_or("output parent")?,
            "fresh canonical output parent",
        )?;
    }
    Ok(path)
}
fn options(args: &[String], names: &[&str]) -> Result<Vec<PathBuf>> {
    require(args.len() == names.len() * 2, "exact Linux option pairs")?;
    args.as_chunks::<2>()
        .0
        .iter()
        .zip(names)
        .map(|([name, value], expected)| {
            require(name == expected, "fixed Linux option order")?;
            path(value, *expected != "--owned-output")
        })
        .collect()
}
fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn save(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC).bits() as i32)
        .open(path)?;
    f.write_all(bytes)?;
    Ok(())
}
fn json_save(path: &Path, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    require(bytes.len() <= 2 * 1024 * 1024, "Linux evidence JSON cap")?;
    save(path, &bytes)
}
fn disjoint(out: &Path, inputs: &[PathBuf]) -> Result<()> {
    require(
        inputs
            .iter()
            .all(|p| !out.starts_with(p) && !p.starts_with(out)),
        "output disjoint from inputs",
    )
}
// Save partial capped streams and actual reaped status before any semantic parse.
fn run(
    root: &Path,
    stem: &str,
    program: &Path,
    args: &[&str],
    cwd: &Path,
    input: Vec<u8>,
    deadline: Duration,
) -> Result<Vec<u8>> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin");
    json_save(
        &root.join(format!("{stem}.command.json")),
        &json!({"program":program,"args":args,"cwd":cwd,"deadline_ms":deadline.as_millis(),"input_bytes":input.len(),"input_sha256":crate::hash(&input)}),
    )?;
    let out = retained::bounded(cmd, input, deadline);
    match out {
        Ok(out) => {
            save(&root.join(format!("{stem}.stdout")), &out.stdout)?;
            save(&root.join(format!("{stem}.stderr")), &out.stderr)?;
            json_save(
                &root.join(format!("{stem}.status.json")),
                &json!({"child_code":out.status.as_ref().and_then(|s|s.code()),"child_status":out.status.as_ref().map(ToString::to_string),"setup_error":out.error,"outer_completed":true}),
            )?;
            require(
                out.error.is_none()
                    && out.status.is_some_and(|s| s.success())
                    && out.stderr.is_empty(),
                "Linux captured child failure retained before parse",
            )?;
            Ok(out.stdout)
        }
        Err(error) => {
            json_save(
                &root.join(format!("{stem}.status.json")),
                &json!({"setup_error":error,"child_code":null,"outer_completed":false}),
            )?;
            save(&root.join(format!("{stem}.stdout")), b"")?;
            save(&root.join(format!("{stem}.stderr")), b"")?;
            Err("Linux pre-spawn/setup failure retained".into())
        }
    }
}
fn git(root: &Path, stem: &str, checkout: &Path, args: &[&str]) -> Result<Vec<u8>> {
    run(
        root,
        stem,
        Path::new("/usr/bin/git"),
        args,
        checkout,
        vec![],
        Duration::from_secs(15),
    )
}
fn core(root: &Path, checkout: &Path) -> Result<()> {
    require(
        git(root, "core-head", checkout, &["rev-parse", "HEAD"])?
            == format!("{}\n", crate::CORE_SHA).as_bytes(),
        "fixed predecessor core HEAD",
    )?;
    require(
        git(root, "core-tree", checkout, &["rev-parse", "HEAD^{tree}"])?
            == format!("{TREE}\n").as_bytes(),
        "fixed predecessor core tree",
    )?;
    require(
        git(root, "core-clean", checkout, &["status", "--porcelain"])?.is_empty(),
        "clean predecessor checkout",
    )
}
fn package_name(name: &str) -> Result<()> {
    let parts: Vec<_> = name.split('/').collect();
    require(
        name.len() <= 256
            && !name.contains('\\')
            && !name.contains("..")
            && !name.contains(char::is_whitespace)
            && if name.starts_with('@') {
                parts.len() == 2 && parts.iter().all(|p| !p.is_empty())
            } else {
                parts.len() == 1 && !name.is_empty()
            },
        "installed dependency name grammar",
    )
}
fn resolve(installed: &Path, from: &Path, name: &str) -> Result<Option<PathBuf>> {
    package_name(name)?;
    let mut at = Some(from);
    while let Some(dir) = at {
        if dir.file_name().is_none_or(|n| n != "node_modules") {
            let candidate = dir.join("node_modules").join(name);
            if candidate.try_exists()? {
                require(
                    fs::symlink_metadata(&candidate)?.is_dir()
                        && fs::canonicalize(&candidate)? == candidate,
                    "canonical installed package directory",
                )?;
                return Ok(Some(candidate));
            }
        }
        if dir == installed {
            break;
        }
        at = dir.parent();
    }
    Ok(None)
}
// One bounded create_new rejection record before refused file reads/copy.
struct InventoryObservation<'a> {
    output: &'a Path,
    installed: &'a Path,
    file: &'a Path,
    metadata: &'a fs::Metadata,
    visited: usize,
    files: usize,
    total: usize,
    pending: usize,
}
impl InventoryObservation<'_> {
    fn gate(&self, ok: bool, reason: &str) -> Result<()> {
        if ok {
            return Ok(());
        }
        let relative = self.file.strip_prefix(self.installed)?;
        let path_bytes = relative.as_os_str().as_bytes();
        let prefix = path_bytes
            .get(..path_bytes.len().min(4096))
            .ok_or("rejection path cap")?;
        let path_hex: String = prefix.iter().map(|b| format!("{b:02x}")).collect();
        let mode = self.metadata.mode();
        let permissions = mode & 0o777;
        let allowed = matches!(permissions, 0o600 | 0o644 | 0o700 | 0o755);
        let planned_total = u64::try_from(self.total)?.checked_add(self.metadata.len());
        let class = if self.metadata.is_file() {
            "regular"
        } else if self.metadata.is_dir() {
            "directory"
        } else if self.metadata.file_type().is_symlink() {
            "symlink"
        } else {
            "special"
        };
        let detail: String = reason.chars().take(512).collect();
        let value = json!({"version":1,"phase":"dependency-inventory-before-read-copy","reason":detail,"reason_truncated":reason.chars().count()>512,"relative_path_utf8":std::str::from_utf8(path_bytes).ok().filter(|_|path_bytes.len()<=256),"relative_path_hex":path_hex,"path_bytes":path_bytes.len(),"path_truncated":path_bytes.len()>4096,"installed_root":self.installed,"bytes":self.metadata.len(),"mode_full_octal":format!("{mode:o}"),"mode_permissions_octal":format!("{permissions:04o}"),"class":class,"nlink":self.metadata.nlink(),"counts_before_insert":{"visited":self.visited,"files":self.files,"total_bytes":self.total,"pending":self.pending,"planned_total_bytes":planned_total},"caps":{"per_file_bytes":16777216,"total_bytes":67108864,"files":4096,"visits":8192,"pending":4096,"path_bytes":4096,"rejection_json_bytes":65536},"failed":{"size":self.metadata.is_file() && self.metadata.len()>16*1024*1024,"source_mode":self.metadata.is_file() && !allowed,"special_bits":mode&0o7000!=0,"class":!self.metadata.is_file() && !self.metadata.is_dir(),"nlink":self.metadata.is_file() && self.metadata.nlink()!=1,"file_count":self.files>=4096,"visit_count":self.visited>8192,"pending_count":self.pending>=4096,"aggregate":self.metadata.is_file() && planned_total.is_none_or(|n|n>64*1024*1024),"nested":reason.starts_with("nested:")},"content_read_for_copy_stage":false,"content_sha256":null,"content_read_scope":"payload copy stage has not read this visit; nested validation may have parsed a root manifest at the same path","core_commit":crate::CORE_SHA,"core_tree":TREE,"fixed_manifest_sha256":{"package.json":"f3f9375884302c7dcdd7fb510b7804f5dadcdced8ccabdc827638378353db19c","package-lock.json":"ad078a36d1f9b72ba7ffeb2fa945deca872ea81742c8fe1ed5c9323ddcf497e9"},"policy":"explicit source0700 umask077 compatibility; original offender unknown; destination0600; unchanged caps"});
        let bytes = serde_json::to_vec_pretty(&value)?;
        require(bytes.len() <= 65536, "rejection metadata cap")?;
        save(&self.output.join("inventory-rejection.json"), &bytes)?;
        Err(
            format!("dependency inventory rejected: {detail}; retained inventory-rejection.json")
                .into(),
        )
    }
}
fn inventory(args: &[String]) -> Result<Value> {
    require(
        cfg!(target_os = "linux"),
        "Linux inventory requires Linux host",
    )?;
    let paths = options(
        args,
        &["--installed-root", "--core-checkout", "--owned-output"],
    )?;
    let [installed, checkout, out]: [PathBuf; 3] =
        paths.try_into().map_err(|_| "inventory paths")?;
    disjoint(&out, &[installed.clone(), checkout.clone()])?;
    private_dir(&out)?;
    core(&out, &checkout)?;
    let mut manifests = Vec::new();
    for (i, (name, sha)) in [
        (
            "package.json",
            "f3f9375884302c7dcdd7fb510b7804f5dadcdced8ccabdc827638378353db19c",
        ),
        (
            "package-lock.json",
            "ad078a36d1f9b72ba7ffeb2fa945deca872ea81742c8fe1ed5c9323ddcf497e9",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let fixed = git(
            &out,
            &format!("manifest-{i}"),
            &checkout,
            &["show", &format!("HEAD:server/{name}")],
        )?;
        require(
            crate::hash(&fixed) == sha && read(&installed.join(name), 1024 * 1024)? == fixed,
            "installed immutable manifest identity",
        )?;
        manifests.push((name, fixed));
    }
    let lock: Value = serde_json::from_slice(&manifests.get(1).ok_or("lock")?.1)?;
    let packages = (*field(&lock, "packages"))
        .as_object()
        .ok_or("fixed lock packages")?;
    let mut pending = Vec::new();
    for name in [
        "@node-rs/argon2",
        "@simplewebauthn/server",
        "better-sqlite3",
        "sqlite-vec",
        "unpdf",
    ] {
        pending.push(resolve(&installed, &installed, name)?.ok_or("required closure package")?);
    }
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut closure = BTreeSet::new();
    while let Some(dir) = pending.pop() {
        require(
            Instant::now() < deadline && closure.len() < 128,
            "closure deadline/package cap",
        )?;
        if !closure.insert(dir.clone()) {
            continue;
        }
        let relative = dir
            .strip_prefix(&installed)?
            .to_str()
            .ok_or("installed relative utf8")?;
        let entry = packages.get(relative).ok_or("installed root absent lock")?;
        let manifest: Value =
            serde_json::from_slice(&read(&dir.join("package.json"), 1024 * 1024)?)?;
        let name = (*field(&manifest, "name"))
            .as_str()
            .ok_or("installed package name")?;
        package_name(name)?;
        require(
            relative.ends_with(&format!("node_modules/{name}"))
                && (*field(&manifest, "version")) == (*field(entry, "version"))
                && (*field(entry, "version")).is_string(),
            "installed package path/name/version",
        )?;
        for dependency_field in ["dependencies", "optionalDependencies"] {
            if let Some(deps) = entry.get(dependency_field).and_then(Value::as_object) {
                for name in deps.keys() {
                    let found = resolve(&installed, &dir, name)?;
                    if let Some(found) = found {
                        if !closure.contains(&found) && !pending.contains(&found) {
                            require(pending.len() < 128, "closure pending cap")?;
                            pending.push(found);
                        }
                    } else {
                        require(
                            dependency_field == "optionalDependencies"
                                || (*field(entry, "optionalDependencies")).get(name).is_some(),
                            "required installed dependency missing",
                        )?;
                    }
                }
            }
        }
    }
    let mut queue: Vec<PathBuf> = closure.iter().cloned().collect();
    let mut files = BTreeMap::new();
    let mut total = 0usize;
    let mut visited = 0usize;
    while let Some(file) = queue.pop() {
        visited += 1;
        let meta = fs::symlink_metadata(&file)?;
        let observation = InventoryObservation {
            output: &out,
            installed: &installed,
            file: &file,
            metadata: &meta,
            visited,
            files: files.len(),
            total,
            pending: queue.len(),
        };
        observation.gate(
            Instant::now() < deadline && visited <= 8192,
            "inventory visit/deadline cap",
        )?;
        observation.gate(!meta.file_type().is_symlink(), "dependency links refused")?;
        observation.gate(
            meta.mode() & 0o7000 == 0,
            "source setuid/setgid/sticky bits refused",
        )?;
        if meta.is_dir() {
            for entry in fs::read_dir(&file)? {
                let pending_observation = InventoryObservation {
                    pending: queue.len(),
                    ..observation
                };
                pending_observation.gate(queue.len() < 4096, "inventory pending cap")?;
                queue.push(entry?.path());
            }
        } else {
            observation.gate(meta.is_file(), "inventory regular file class")?;
            observation.gate(meta.nlink() == 1, "inventory singleton link count")?;
            observation.gate(files.len() < 4096, "inventory file count cap")?;
            let relative = file.strip_prefix(&installed)?.to_path_buf();
            if files.contains_key(&relative) {
                continue;
            }
            observation.gate(
                meta.len() <= 16 * 1024 * 1024,
                "inventory per-file size cap",
            )?;
            // Requested0755 under fixed umask077 can produce0700. This policy
            // does not identify the original first failure's unretained mode.
            observation.gate(
                matches!(meta.mode() & 0o777, 0o600 | 0o644 | 0o700 | 0o755),
                "inventory source permission mode",
            )?;
            let next_total = total.checked_add(usize::try_from(meta.len())?);
            observation.gate(
                next_total.is_some_and(|n| n <= 64 * 1024 * 1024),
                "inventory total cap before read",
            )?;
            // Every nested installation encountered during whole-package copying is locked,
            // even when it was not independently selected by graph resolution.
            let nested = (|| -> Result<()> {
                let components: Vec<_> = relative.components().collect();
                for (index, c) in components.iter().enumerate() {
                    if *c == Component::Normal("node_modules".as_ref()) {
                        let next = components.get(index + 1).ok_or("installation name")?;
                        let scoped = next
                            .as_os_str()
                            .to_str()
                            .ok_or("installation utf8")?
                            .starts_with('@');
                        let end = index + if scoped { 3 } else { 2 };
                        require(end <= components.len(), "installation boundary")?;
                        let root: PathBuf = components.iter().take(end).collect();
                        let key = root.to_str().ok_or("installation path utf8")?;
                        let locked = packages
                            .get(key)
                            .ok_or("unlisted nested installation root")?;
                        let actual: Value = serde_json::from_slice(&read(
                            &installed.join(&root).join("package.json"),
                            1024 * 1024,
                        )?)?;
                        let rootname = components
                            .iter()
                            .skip(index + 1)
                            .take(if scoped { 2 } else { 1 })
                            .map(|c| c.as_os_str().to_str().ok_or("installation name utf8"))
                            .collect::<std::result::Result<Vec<_>, _>>()?
                            .join("/");
                        require(
                            (*field(&actual, "name")) == rootname
                                && (*field(&actual, "version")) == (*field(locked, "version"))
                                && (*field(locked, "version")).is_string(),
                            "nested installation identity",
                        )?;
                    }
                }
                Ok(())
            })();
            if let Err(error) = nested {
                observation.gate(false, &format!("nested:{error}"))?;
            }
            total = next_total.ok_or("inventory total overflow after gate")?;
            let bytes = read(&file, 16 * 1024 * 1024)?;
            require(
                bytes.len() == usize::try_from(meta.len())?,
                "stable dependency size",
            )?;
            files.insert(relative, (bytes, format!("100{:03o}", meta.mode() & 0o777)));
        }
    }
    let destination = out.join("dependencies");
    private_dir(&destination)?;
    let mut rows = Vec::new();
    let mut sources = Vec::new();
    for (relative, (bytes, mode)) in files {
        require(
            Instant::now() < deadline,
            "private copy cooperative deadline",
        )?;
        let target = destination.join(&relative);
        let parent = target.parent().ok_or("copy parent")?;
        fs::create_dir_all(parent)?;
        let mut dir = parent;
        loop {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
            if dir == destination {
                break;
            }
            dir = dir.parent().ok_or("copy ancestry")?;
        }
        save(&target, &bytes)?;
        require(
            read(&target, 16 * 1024 * 1024)? == bytes
                && fs::symlink_metadata(&target)?.mode() & 0o777 == 0o600,
            "copied identity/mode",
        )?;
        rows.push(json!({"path":relative,"bytes":bytes.len(),"sha256":crate::hash(&bytes),"mode":"100600"}));
        sources.push(json!({"path":relative,"source_mode":mode,"source_sha256":crate::hash(&bytes),"destination_mode":"100600"}));
    }
    for (name, bytes) in manifests {
        save(&destination.join(name), &bytes)?;
    }
    let proof = json!({"files":rows,"source":installed,"destination":destination,"source_attestation":sources,"total_bytes":total,"install_or_download":false,"limits":"stable trusted ancestors; no registry or dylib ancestry proof"});
    json_save(&out.join("dependency-inventory.json"), &proof)?;
    Ok(proof)
}

fn text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| "qualification path utf8".into())
}
fn binary_copy(source: &Path, target: &Path) -> Result<String> {
    let bytes = read(source, 128 * 1024 * 1024)?;
    require(!bytes.is_empty(), "nonempty executable")?;
    save(target, &bytes)?;
    fs::set_permissions(target, fs::Permissions::from_mode(0o700))?;
    Ok(crate::hash(&bytes))
}
fn check_hash(path: &Path, sha: &str, cap: usize) -> Result<()> {
    require(
        crate::hash(&read(path, cap)?) == sha,
        "execution identity unchanged",
    )
}
// Stream source content with an 8KiB buffer. No whole-tree byte allocation.
fn source_digest(path: &Path, remaining: u64) -> Result<(u64, String, u32)> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC)
                .bits() as i32,
        )
        .open(path)?;
    let metadata = file.metadata()?;
    let cap = remaining.min(16 * 1024 * 1024);
    require(
        metadata.is_file() && metadata.nlink() == 1 && metadata.len() <= cap,
        "streamed source class/perfile/remaining cap",
    )?;
    let mut digest = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(u64::try_from(count)?)
            .ok_or("streamed source overflow")?;
        require(total <= cap, "streamed source complete cap")?;
        digest.update(buffer.get(..count).ok_or("streamed source read count")?);
    }
    require(total == metadata.len(), "streamed source stable length")?;
    Ok((
        total,
        format!("{:x}", digest.finalize()),
        metadata.mode() & 0o777,
    ))
}
fn source_identity(root: &Path, source: &Path) -> Result<Value> {
    let head = git(root, "source-head", source, &["rev-parse", "HEAD"])?;
    let tree = git(root, "source-tree", source, &["rev-parse", "HEAD^{tree}"])?;
    require(
        git(root, "source-clean", source, &["status", "--porcelain"])?.is_empty(),
        "clean committed qualification source",
    )?;
    let inventory = git(
        root,
        "source-tracked",
        source,
        &["ls-tree", "-rz", "--full-tree", "HEAD"],
    )?;
    require(
        inventory.len() <= 2 * 1024 * 1024,
        "source inventory bytes cap",
    )?;
    let mut rows = Vec::new();
    let mut total = 0u64;
    for entry in inventory.split(|c| *c == 0).filter(|r| !r.is_empty()) {
        require(rows.len() < 4096, "source inventory count cap")?;
        let entry = std::str::from_utf8(entry)?;
        let (identity, name) = entry.split_once('\t').ok_or("source tree record")?;
        let fields: Vec<_> = identity.split(' ').collect();
        let [mode, kind, blob] = fields.as_slice() else {
            return Err("source tree identity".into());
        };
        require(
            *kind == "blob" && matches!(*mode, "100644" | "100755"),
            "source regular tracked blob",
        )?;
        let relative = Path::new(name);
        require(
            relative
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
            "source relative grammar",
        )?;
        // Current committed tree is ~82MiB; dependency caps remain unchanged.
        let remaining = (128u64 * 1024 * 1024)
            .checked_sub(total)
            .ok_or("source total cap")?;
        let (bytes, sha, actual_mode) = source_digest(&source.join(relative), remaining)?;
        total = total.checked_add(bytes).ok_or("source total overflow")?;
        require(total <= 128 * 1024 * 1024, "source total cap")?;
        require(
            format!("100{actual_mode:03o}") == *mode,
            "source actual tracked mode",
        )?;
        rows.push(json!({"path":name,"git_blob":blob,"mode":mode,"bytes":bytes,"sha256":sha}));
    }
    let value = json!({"head":std::str::from_utf8(&head)?.trim_end(),"tree":std::str::from_utf8(&tree)?.trim_end(),"files":rows});
    json_save(&root.join("source-identity.json"), &value)?;
    Ok(value)
}
fn source_check(source: &Path, identity: &Value) -> Result<()> {
    for row in (*field(identity, "files"))
        .as_array()
        .ok_or("source identity files")?
    {
        let name = (*field(row, "path")).as_str().ok_or("source path")?;
        let (bytes, sha, actual_mode) = source_digest(&source.join(name), 16 * 1024 * 1024)?;
        require(
            (*field(row, "sha256")) == sha
                && (*field(row, "bytes")) == bytes
                && (*field(row, "mode")) == format!("100{actual_mode:03o}"),
            "source at execution identity",
        )?;
    }
    Ok(())
}
fn qualify(args: &[String]) -> Result<Value> {
    let paths = options(
        args,
        &[
            "--core-checkout",
            "--dependency-root",
            "--dependency-inventory",
            "--node",
            "--lib-test",
            "--source-checkout",
            "--owned-output",
        ],
    )?;
    let [checkout, deps, inventory, node, lib, source, out]: [PathBuf; 7] =
        paths.try_into().map_err(|_| "coordinator paths")?;
    disjoint(
        &out,
        &[
            checkout.clone(),
            deps.clone(),
            inventory.clone(),
            node.clone(),
            lib.clone(),
            source.clone(),
        ],
    )?;
    private_dir(&out)?;
    let operation = (|| -> Result<Value> {
        require(
            cfg!(target_os = "linux"),
            "Linux qualification requires actual Linux host",
        )?;
        core(&out, &checkout)?;
        let identity = source_identity(&out, &source)?;
        let executable = fs::canonicalize(std::env::current_exe()?)?;
        let tool_sha = binary_copy(&executable, &out.join("executed-coordinator.bin"))?;
        let lib_owned = out.join("lib-test");
        let lib_sha = binary_copy(&lib, &lib_owned)?;
        let node_sha = crate::hash(&read(&node, 128 * 1024 * 1024)?);
        save(
            &out.join("node-preexecution.bin"),
            &read(&node, 128 * 1024 * 1024)?,
        )?;
        let inventory_sha = crate::hash(&read(&inventory, 2 * 1024 * 1024)?);
        json_save(
            &out.join("execution-inputs.json"),
            &json!({"coordinator":executable,"coordinator_sha256":tool_sha,"lib_source":lib,"lib_sha256":lib_sha,"node":node,"node_sha256":node_sha,"dependency_inventory":inventory,"inventory_sha256":inventory_sha,"source_head":(*field(&identity, "head")),"source_tree":(*field(&identity, "tree"))}),
        )?;
        source_check(&source, &identity)?;
        check_hash(&executable, &tool_sha, 128 * 1024 * 1024)?;
        check_hash(&node, &node_sha, 128 * 1024 * 1024)?;
        let metadata = run(
            &out,
            "node-metadata",
            &node,
            &["-p", METADATA],
            &out,
            vec![],
            Duration::from_secs(15),
        )?;
        require(metadata.len() <= 1024, "Node metadata cap")?;
        let runtime: Value = serde_json::from_slice(&metadata)?;
        require(
            (*field(&runtime, "version")) == "v22.23.3"
                && (*field(&runtime, "abi")) == "127"
                && (*field(&runtime, "platform")) == "linux"
                && (*field(&runtime, "arch"))
                    .as_str()
                    .is_some_and(|a| matches!(a, "x64" | "arm64")),
            "actual Linux Node native ABI dialect",
        )?;
        check_hash(&node, &node_sha, 128 * 1024 * 1024)?;
        check_hash(&inventory, &inventory_sha, 2 * 1024 * 1024)?;
        let oracle = out.join("oracle");
        let materialize_args = [
            "--core-checkout",
            text(&checkout)?,
            "--dependency-root",
            text(&deps)?,
            "--dependency-inventory",
            text(&inventory)?,
            "--node",
            text(&node)?,
            "--owned-output",
            text(&oracle)?,
        ]
        .map(str::to_owned);
        let counter = Cell::new(0usize);
        let invoker =
            |program: &Path, argv: &[&str], cwd: &Path, deadline: Duration| -> Result<Vec<u8>> {
                require(
                    program == Path::new("/usr/bin/git"),
                    "fixed materializer evidence program",
                )?;
                let count = counter.get();
                require(count < 1030, "materializer Git invocation cap")?;
                counter.set(count + 1);
                run(
                    &out,
                    &format!("materialize-git-{count:04}"),
                    program,
                    argv,
                    cwd,
                    vec![],
                    deadline,
                )
            };
        let manifest = super::chatgpt_status_dependencies::materialize_with_runtime(
            &materialize_args,
            Some(&metadata),
            Some(&invoker),
        )?;
        require(
            (*field(&manifest, "node_sha256")) == node_sha
                && (*field(&manifest, "runtime")) == runtime,
            "materialized runtime preflight identity",
        )?;
        let rows = (*field(&manifest, "files"))
            .as_array()
            .ok_or("materialized files")?;
        require(rows.len() <= 5120, "materialized inventory cap")?;
        let topology_check = || -> Result<()> {
            for row in rows {
                let name = field(row, "path").as_str().ok_or("materialized path")?;
                let file = oracle.join(name);
                let metadata = fs::symlink_metadata(&file)?;
                require(
                    metadata.is_file() && metadata.nlink() == 1 && metadata.mode() & 0o777 == 0o600,
                    "materialized preexecution class/mode",
                )?;
                check_hash(
                    &file,
                    field(row, "sha256").as_str().ok_or("materialized SHA")?,
                    16 * 1024 * 1024,
                )?;
                require(
                    metadata.len() == field(row, "bytes").as_u64().ok_or("materialized size")?,
                    "materialized preexecution bytes",
                )?;
            }
            Ok(())
        };
        topology_check()?;
        // Source-fixed native loads; memory database only, no application index/import.
        const NATIVE:&str="const path=require('path'); const root=process.cwd(); const Database=require(path.join(root,'node_modules/better-sqlite3')); const db=new Database(':memory:'); try { require(path.join(root,'node_modules/@node-rs/argon2')); require(path.join(root,'node_modules/sqlite-vec')).load(db); process.stdout.write(JSON.stringify({version:process.version,abi:process.versions.modules,platform:process.platform,native:'better-sqlite3+argon2+sqlite-vec'})); } finally { db.close(); }";
        source_check(&source, &identity)?;
        check_hash(&node, &node_sha, 128 * 1024 * 1024)?;
        let native = run(
            &out,
            "native-preflight",
            &node,
            &["-e", NATIVE],
            &oracle,
            vec![],
            Duration::from_secs(15),
        )?;
        let native: Value = serde_json::from_slice(&native)?;
        require(
            native
                == json!({"version":"v22.23.3","abi":"127","platform":"linux","native":"better-sqlite3+argon2+sqlite-vec"}),
            "actual native loads preflight",
        )?;
        let input = read(&oracle.join("qualification-input.json"), 8192)?;
        require(
            crate::hash(&input) == (*field(&manifest, "input_sha256"))
                && crate::hash(crate::ADAPTER.as_bytes()) == (*field(&manifest, "adapter_sha256")),
            "capture input/adapter preexecution identity",
        )?;
        source_check(&source, &identity)?;
        check_hash(&node, &node_sha, 128 * 1024 * 1024)?;
        topology_check()?;
        let stdout = run(
            &oracle,
            "capture",
            &node,
            &["-e", crate::ADAPTER],
            &oracle,
            input,
            Duration::from_secs(15),
        )?;
        require(stdout.len() <= 1024 * 1024, "capture output cap")?;
        let raw: Value = serde_json::from_slice(&stdout)?;
        require(
            (*field(&raw, "node")) == "v22.23.3" && (*field(&raw, "abi")) == "127",
            "capture dialect",
        )?;
        let modules = (*field(&raw, "modules"))
            .as_array()
            .ok_or("capture modules")?;
        require(
            !modules.is_empty() && modules.len() <= 512,
            "capture finite loaded count",
        )?;
        let mut unique = BTreeSet::new();
        let mut loaded = Vec::new();
        for module in modules {
            let name = module.as_str().ok_or("loaded filename")?;
            require(
                name.len() <= 4096 && unique.insert(name),
                "bounded unique loaded filename",
            )?;
            let loaded_path = Path::new(name);
            require(
                fs::canonicalize(loaded_path)? == loaded_path,
                "canonical loaded source",
            )?;
            let relative = loaded_path
                .strip_prefix(&oracle)?
                .to_str()
                .ok_or("loaded relative")?;
            let row = rows
                .iter()
                .find(|r| (*field(r, "path")) == relative)
                .ok_or("loaded absent source manifest")?;
            let bytes = read(loaded_path, 16 * 1024 * 1024)?;
            require(
                crate::hash(&bytes) == (*field(row, "sha256"))
                    && bytes.len() == (*field(row, "bytes")),
                "loaded actual identity",
            )?;
            loaded.push(row.clone());
        }
        let cases = (*field(&raw, "cases")).as_array().ok_or("capture cases")?;
        require(
            cases.len() == 7 && cases.iter().zip(IDS).all(|(r, id)| (*field(r, "id")) == id),
            "capture seven exact ids",
        )?;
        let certificate = json!({"manifest":manifest,"stdout_sha256":crate::hash(&stdout),"stderr_sha256":crate::hash(b""),"raw_exit_code":0,"loaded_files":loaded,"cases":cases,"qualification":"fresh Linux finite predecessor factory; no public index/header/defaultoff/supervisor/registry/dylib ancestry proof"});
        json_save(&oracle.join("capture-certificate.json"), &certificate)?;
        let schema =
            include_str!("../../../../../crates/server-store/tests/fixtures/node-schema.sql");
        let binding = json!({"version":1,"root":oracle,"capture_sha256":crate::hash(&stdout),"manifest_sha256":crate::hash(&read(&oracle.join("qualification-manifest.json"),2*1024*1024)?),"certificate_sha256":crate::hash(&read(&oracle.join("capture-certificate.json"),2*1024*1024)?),"core_commit":crate::CORE_SHA,"core_tree":TREE,"now":1800000000000i64,"input_sha256":(*field(&manifest, "input_sha256")),"adapter_sha256":(*field(&manifest, "adapter_sha256")),"schema_sha256":crate::hash(schema.as_bytes()),"ids":IDS,"runtime":runtime});
        json_save(&out.join("m6-ci-reference-binding.json"), &binding)?;
        let binding_sha = crate::hash(&read(&out.join("m6-ci-reference-binding.json"), 8192)?);
        let attest = || -> Result<()> {
            source_check(&source, &identity)?;
            check_hash(&executable, &tool_sha, 128 * 1024 * 1024)?;
            check_hash(&lib_owned, &lib_sha, 128 * 1024 * 1024)?;
            check_hash(
                &out.join("m6-ci-reference-binding.json"),
                &binding_sha,
                8192,
            )
        };
        attest()?;
        let listed = run(
            &out,
            "lib-list",
            &lib_owned,
            &["--list"],
            &out,
            vec![],
            Duration::from_secs(45),
        )?;
        let listed = std::str::from_utf8(&listed)?;
        let tests: Vec<_> = listed
            .lines()
            .filter_map(|line| line.strip_suffix(": test"))
            .collect();
        require(
            !tests.is_empty()
                && tests.len() <= 2048
                && tests.iter().filter(|n| **n == NEGATIVE).count() == 1
                && tests.iter().filter(|n| **n == POSITIVE).count() == 1,
            "actual full finite lib inventory contains exact controls",
        )?;
        attest()?;
        let negative = run(
            &out,
            "lib-negative",
            &lib_owned,
            &[NEGATIVE, "--exact", "--nocapture"],
            &out,
            vec![],
            Duration::from_secs(45),
        )?;
        let negative = std::str::from_utf8(&negative)?;
        require(
            negative.contains("M6_LINUX_NEGATIVE grants.last_used_at +1 rejected")
                && negative.contains("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured;")
                && negative.contains(&format!("{} filtered out", tests.len() - 1)),
            "semantic timestamp negative and exact filtered inventory",
        )?;
        attest()?;
        let positive = run(
            &out,
            "lib-positive",
            &lib_owned,
            &[POSITIVE, "--exact", "--nocapture"],
            &out,
            vec![],
            Duration::from_secs(45),
        )?;
        let positive = std::str::from_utf8(&positive)?;
        require(
            positive.contains("M6_CURRENT_RUST_HTTP_OBSERVATIONS ")
                && positive.contains("M6_CURRENT_FACTORY_ROWS ")
                && positive.contains("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured;")
                && positive.contains(&format!("{} filtered out", tests.len() - 1)),
            "seven row positive and exact filtered inventory",
        )?;
        attest()?;
        let cert = json!({"source":identity,"coordinator_sha256":tool_sha,"lib_sha256":lib_sha,"node_sha256":node_sha,"binding_sha256":binding_sha,"full_test_count":tests.len(),"negative":NEGATIVE,"positive":POSITIVE,"passed":true,"scope":"seven synthetic Rust router rows versus CORE010735 factory; independent Rust headers401/403; public Node headers/index/current0a204/defaultoff/supervisor/fullM6 remain held"});
        json_save(&out.join("linux-qualification-certificate.json"), &cert)?;
        Ok(cert)
    })();
    json_save(
        &out.join("coordinator-outcome.json"),
        &match &operation {
            Ok(_) => json!({"outer_code":0,"completed":true}),
            Err(e) => {
                json!({"outer_code":1,"completed":false,"typed_failure":"setup-or-child-qualification-failure","message":e.to_string()})
            }
        },
    )?;
    operation
}
// The worker seam has five independent databases and no status-factory invocation.
fn capture_worker(args: &[String]) -> Result<Value> {
    use super::chatgpt_worker_auth_oracle as worker;
    let paths = options(
        args,
        &[
            "--core-checkout",
            "--dependency-root",
            "--dependency-inventory",
            "--node",
            "--source-checkout",
            "--owned-output",
        ],
    )?;
    let [checkout, deps, inventory, node, source, out]: [PathBuf; 6] =
        paths.try_into().map_err(|_| "worker paths")?;
    disjoint(
        &out,
        &[
            checkout.clone(),
            deps.clone(),
            inventory.clone(),
            node.clone(),
            source.clone(),
        ],
    )?;
    private_dir(&out)?;
    let operation = (|| -> Result<Value> {
        require(
            cfg!(target_os = "linux"),
            "worker capture actual Linux only",
        )?;
        let deadline = Instant::now() + Duration::from_secs(120);
        core(&out, &checkout)?;
        let identity = source_identity(&out, &source)?;
        worker::source_binding(&identity)?;
        let job = worker::expected_identity(&out, &identity)?;
        let executable = fs::canonicalize(std::env::current_exe()?)?;
        let tool_sha = binary_copy(&executable, &out.join("executed-coordinator.bin"))?;
        let node_sha = crate::hash(&read(&node, 128 * 1024 * 1024)?);
        save(
            &out.join("node-preexecution.bin"),
            &read(&node, 128 * 1024 * 1024)?,
        )?;
        let inventory_sha = crate::hash(&read(&inventory, 2 * 1024 * 1024)?);
        json_save(
            &out.join("execution-inputs.json"),
            &json!({"job":job,"root":out,"compiled_map":worker::compiled_map(),"coordinator":executable,"coordinator_sha256":tool_sha,"node":node,"node_sha256":node_sha,"inventory_sha256":inventory_sha,"source":identity,"adapter_sha256":worker::hash(worker::ADAPTER.as_bytes()),"schema_sha256":worker::hash(worker::SCHEMA.as_bytes()),"setup_sha256":worker::hash(worker::SETUP.as_bytes())}),
        )?;
        let attest = || -> Result<()> {
            require(
                Instant::now() < deadline,
                "worker overall cooperative deadline",
            )?;
            source_check(&source, &identity)?;
            check_hash(&executable, &tool_sha, 128 * 1024 * 1024)?;
            check_hash(&node, &node_sha, 128 * 1024 * 1024)?;
            check_hash(&inventory, &inventory_sha, 2 * 1024 * 1024)
        };
        attest()?;
        let metadata = run(
            &out,
            "node-metadata",
            &node,
            &["-p", METADATA],
            &out,
            vec![],
            Duration::from_secs(15),
        )?;
        require(metadata.len() <= 1024, "worker metadata cap")?;
        let runtime: Value = serde_json::from_slice(&metadata)?;
        require(
            field(&runtime, "version") == "v22.23.3"
                && field(&runtime, "abi") == "127"
                && field(&runtime, "platform") == "linux"
                && field(&runtime, "arch")
                    .as_str()
                    .is_some_and(|a| matches!(a, "x64" | "arm64")),
            "worker Linux dialect",
        )?;
        attest()?;
        let oracle = out.join("oracle");
        let materialize_args = [
            "--core-checkout",
            text(&checkout)?,
            "--dependency-root",
            text(&deps)?,
            "--dependency-inventory",
            text(&inventory)?,
            "--node",
            text(&node)?,
            "--owned-output",
            text(&oracle)?,
        ]
        .map(str::to_owned);
        let counter = Cell::new(0usize);
        let invoker =
            |program: &Path, argv: &[&str], cwd: &Path, limit: Duration| -> Result<Vec<u8>> {
                require(
                    program == Path::new("/usr/bin/git"),
                    "worker materializer fixed Git",
                )?;
                let count = counter.get();
                require(count < 1030, "worker Git count cap")?;
                counter.set(count + 1);
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or("worker deadline")?;
                run(
                    &out,
                    &format!("materialize-git-{count:04}"),
                    program,
                    argv,
                    cwd,
                    vec![],
                    limit.min(remaining),
                )
            };
        let manifest = super::chatgpt_status_dependencies::materialize_with_runtime(
            &materialize_args,
            Some(&metadata),
            Some(&invoker),
        )?;
        require(
            field(&manifest, "node_sha256").as_str() == Some(node_sha.as_str())
                && field(&manifest, "runtime") == &runtime,
            "worker materialized runtime",
        )?;
        let rows = field(&manifest, "files")
            .as_array()
            .ok_or("worker materialized rows")?;
        require(rows.len() <= 5120, "worker materialized count")?;
        let topology = || -> Result<()> {
            for row in rows {
                let file = oracle.join(field(row, "path").as_str().ok_or("worker manifest path")?);
                let m = fs::symlink_metadata(&file)?;
                require(
                    m.is_file()
                        && m.nlink() == 1
                        && m.mode() & 0o7777 == 0o600
                        && Some(m.len()) == field(row, "bytes").as_u64(),
                    "worker file class/mode/size",
                )?;
                check_hash(
                    &file,
                    field(row, "sha256").as_str().ok_or("worker file SHA")?,
                    16 * 1024 * 1024,
                )?;
            }
            Ok(())
        };
        let trace = |raw: &Value| -> Result<Vec<Value>> {
            let modules = field(raw, "modules")
                .as_array()
                .ok_or("worker actual modules")?;
            require(
                !modules.is_empty() && modules.len() <= 512,
                "worker actual loaded cap",
            )?;
            let mut unique = BTreeSet::new();
            let mut loaded = Vec::new();
            for name in modules {
                let name = name.as_str().ok_or("worker module path")?;
                require(
                    name.len() <= 4096 && unique.insert(name),
                    "worker unique bounded module",
                )?;
                let file = Path::new(name);
                require(
                    fs::canonicalize(file)? == file,
                    "worker canonical loaded path",
                )?;
                let relative = file
                    .strip_prefix(&oracle)?
                    .to_str()
                    .ok_or("worker relative module")?;
                let row = rows
                    .iter()
                    .find(|r| field(r, "path") == relative)
                    .ok_or("worker unlisted loaded module")?;
                let m = fs::symlink_metadata(file)?;
                require(
                    m.is_file()
                        && m.nlink() == 1
                        && m.mode() & 0o7777 == 0o600
                        && Some(m.len()) == field(row, "bytes").as_u64(),
                    "worker loaded metadata",
                )?;
                check_hash(
                    file,
                    field(row, "sha256").as_str().ok_or("worker loaded SHA")?,
                    16 * 1024 * 1024,
                )?;
                loaded.push(row.clone());
            }
            Ok(loaded)
        };
        const NATIVE: &str="const path=require('path'); const root=process.cwd(); const Database=require(path.join(root,'node_modules/better-sqlite3')); const db=new Database(':memory:'); try { require(path.join(root,'node_modules/@node-rs/argon2')); require(path.join(root,'node_modules/sqlite-vec')).load(db); } finally { db.close(); } process.stdout.write(JSON.stringify({version:process.version,abi:process.versions.modules,platform:process.platform,modules:Object.keys(require.cache).sort()}));";
        attest()?;
        topology()?;
        let native = run(
            &out,
            "native-preflight",
            &node,
            &["-e", NATIVE],
            &oracle,
            vec![],
            Duration::from_secs(15),
        )?;
        require(native.len() <= 1024 * 1024, "worker native output cap")?;
        let native: Value = serde_json::from_slice(&native)?;
        require(
            field(&native, "version") == "v22.23.3"
                && field(&native, "abi") == "127"
                && field(&native, "platform") == "linux",
            "worker actual native dialect",
        )?;
        let native_loaded = trace(&native)?;
        let fixtures = oracle.join("worker-fixtures");
        private_dir(&fixtures)?;
        save(&out.join("worker-adapter.cjs"), worker::ADAPTER.as_bytes())?;
        let mut result = Vec::new();
        let mut captures = Vec::new();
        for ((cookie, bearer), id) in [
            ("cowork_session=live", ""),
            ("cowork_session=idle", ""),
            ("", "Bearer nva_device"),
            ("cowork_session=live", "Bearer nva_device"),
            ("", ""),
        ]
        .into_iter()
        .zip(worker::IDS)
        {
            let directory = fixtures.join(id);
            private_dir(&directory)?;
            save(&directory.join("secrets.key"), &[7; 32])?;
            let input = serde_json::to_vec(
                &json!({"checkout":oracle,"dataDir":directory,"cookie":cookie,"bearer":bearer,"schema":worker::SCHEMA,"setup":worker::setup_sql()}),
            )?;
            require(input.len() <= 64 * 1024, "worker fixed input cap")?;
            save(&out.join(format!("input-{id}.json")), &input)?;
            attest()?;
            topology()?;
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or("worker deadline")?;
            let raw = run(
                &out,
                &format!("capture-{id}"),
                &node,
                &["-e", worker::ADAPTER],
                &oracle,
                input,
                remaining.min(Duration::from_secs(15)),
            )?;
            require(raw.len() <= 1024 * 1024, "worker semantic output cap")?;
            let value: Value = serde_json::from_slice(&raw)?;
            require(
                field(&value, "node") == "v22.23.3"
                    && field(&value, "abi") == "127"
                    && field(&value, "platform") == "linux",
                "worker capture dialect",
            )?;
            let loaded = trace(&value)?;
            let mut payload = field(&value, "payload").clone();
            payload
                .as_object_mut()
                .ok_or("worker payload object")?
                .insert("id".into(), json!(id));
            result.push(payload);
            captures.push(json!({"id":id,"stdout_sha256":crate::hash(&raw),"stderr_sha256":crate::hash(b""),"status_sha256":crate::hash(&read(&out.join(format!("capture-{id}.status.json")),8192)?),"loaded":loaded,"input_sha256":crate::hash(&read(&out.join(format!("input-{id}.json")),64*1024)?)}));
            attest()?;
            topology()?;
        }
        worker::validate(&result)?;
        json_save(&out.join("worker-reference.json"), &json!(result))?;
        let certificate = json!({"core_commit":worker::CORE,"adapter_sha256":worker::hash(worker::ADAPTER.as_bytes()),"rows":result,"captures":captures,"native_loaded":native_loaded,"native_stdout_sha256":crate::hash(&read(&out.join("native-preflight.stdout"),1024*1024)?),"native_stderr_sha256":crate::hash(b""),"native_status_sha256":crate::hash(&read(&out.join("native-preflight.status.json"),8192)?),"scope":"five independent predecessor auth/device factory methods; no Node public headers/index/current core/defaultoff/supervisor/fullM6/native dylib ancestry proof"});
        json_save(&out.join("worker-certificate.json"), &certificate)?;
        let mut binding = json!({"version":2,"root":out,"core_commit":worker::CORE,"core_tree":worker::TREE,"now":worker::NOW,"ids":worker::IDS,"runtime":runtime,"node_sha256":node_sha,"schema_sha256":worker::hash(worker::SCHEMA.as_bytes()),"setup_sha256":worker::hash(worker::SETUP.as_bytes()),"adapter_sha256":worker::hash(worker::ADAPTER.as_bytes()),"test_sha256":worker::hash(include_bytes!("../../../../../bins/noevia-server/tests/chatgpt_worker.rs")),"certificate_sha256":crate::hash(&read(&out.join("worker-certificate.json"),2*1024*1024)?),"manifest_sha256":crate::hash(&read(&oracle.join("qualification-manifest.json"),2*1024*1024)?),"reference_sha256":crate::hash(&read(&out.join("worker-reference.json"),1024*1024)?),"source_head":field(&identity,"head"),"source_tree":field(&identity,"tree"),"tool_sha256":tool_sha});
        let object = binding.as_object_mut().ok_or("worker binding object")?;
        object.insert("job".into(), json!(job));
        object.insert(
            "compiled_map_sha256".into(),
            json!(crate::hash(&serde_json::to_vec(&worker::compiled_map())?)),
        );
        object.insert(
            "source_identity_sha256".into(),
            json!(crate::hash(&read(
                &out.join("source-identity.json"),
                2 * 1024 * 1024
            )?)),
        );
        object.insert(
            "execution_inputs_sha256".into(),
            json!(crate::hash(&read(
                &out.join("execution-inputs.json"),
                2 * 1024 * 1024
            )?)),
        );
        worker::reference_provisional(&out, &binding)?;
        attest()?;
        topology()?;
        require(
            Instant::now() < deadline,
            "worker final cooperative deadline",
        )?;
        // No consumable terminal or binding has been published before all checks.
        Ok(binding)
    })();
    match operation {
        Ok(mut binding) => {
            // A directed binding -> terminal -> evidence graph avoids circular hashes.
            let mut terminal = json!({"outer_code":0,"completed":true});
            let object = terminal.as_object_mut().ok_or("worker terminal object")?;
            for key in [
                "job",
                "root",
                "source_head",
                "source_tree",
                "reference_sha256",
                "certificate_sha256",
                "manifest_sha256",
                "source_identity_sha256",
                "execution_inputs_sha256",
                "tool_sha256",
                "compiled_map_sha256",
            ] {
                object.insert(key.into(), field(&binding, key).clone());
            }
            json_save(&out.join("worker-outcome.json"), &terminal)?;
            binding
                .as_object_mut()
                .ok_or("worker binding object")?
                .insert(
                    "outcome_sha256".into(),
                    json!(crate::hash(&read(&out.join("worker-outcome.json"), 16384)?)),
                );
            // Publication is the final operation. Failed runs retain raw partial files.
            json_save(&out.join("worker-binding.json"), &binding)?;
            Ok(binding)
        }
        Err(error) => {
            json_save(
                &out.join("worker-outcome.json"),
                &json!({"outer_code":1,"completed":false,"typed_failure":"worker-setup-or-child-failure","message":error.to_string()}),
            )?;
            Err(error)
        }
    }
}

pub fn dispatch(args: &[String]) -> Result<Value> {
    match args.split_first() {
        Some((name, args)) if name == "inventory-linux-dependencies" => inventory(args),
        Some((name, args)) if name == "qualify-linux-factory" => qualify(args),
        Some((name, args)) if name == "capture-linux-worker-auth" => capture_worker(args),
        _ => Err("fixed Linux qualification subcommand".into()),
    }
}

mod retained {
    use std::{
        io::{Read, Write},
        os::fd::AsFd,
        os::unix::process::CommandExt,
        process::{Child, Command, ExitStatus, Stdio},
        thread,
        time::{Duration, Instant},
    };
    pub const INPUT: usize = 8 * 1024 * 1024;
    const OUTPUT: usize = 8 * 1024 * 1024;
    const STDERR: usize = 64 * 1024;
    struct Owned(Child, bool);
    impl Owned {
        fn pid(&self) -> Result<rustix::process::Pid, String> {
            i32::try_from(self.0.id())
                .ok()
                .and_then(rustix::process::Pid::from_raw)
                .ok_or_else(|| "invalid child identity".into())
        }
        fn exited(&self) -> Result<bool, String> {
            use rustix::process::{waitid, WaitId, WaitIdOptions};
            waitid(
                WaitId::Pid(self.pid()?),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            )
            .map(|status| status.is_some())
            .map_err(|e| format!("child observation failed: {e}"))
        }
        fn stop(&mut self, before_signal: impl FnOnce(u32)) -> Result<ExitStatus, String> {
            if self.1 {
                return Err("child already reaped".into());
            }
            // No try_wait/wait has consumed the leader. Even an exited leader retains
            // this numeric identity until the wait below, after the owned group signal.
            before_signal(self.0.id());
            if let Ok(pid) = self.pid() {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
            let _ = self.0.kill();
            let status = self.0.wait().map_err(|e| format!("child reap failed: {e}"));
            self.1 = status.is_ok();
            status
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            if !self.1 {
                let _ = self.stop(|_| {});
            }
        }
    }
    fn nonblocking(pipe: &impl AsFd) -> Result<(), String> {
        let flags = rustix::fs::fcntl_getfl(pipe).map_err(|e| e.to_string())?;
        rustix::fs::fcntl_setfl(pipe, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(|e| e.to_string())
    }
    // One bounded syscall per stream per iteration prevents a continuously producing
    // child from starving another stream or the monotonic deadline check.
    fn read_step(pipe: &mut impl Read, bytes: &mut Vec<u8>, cap: usize) -> Result<bool, String> {
        let mut buf = [0; 4096];
        match pipe.read(&mut buf) {
            Ok(0) => Ok(true),
            Ok(n) => {
                if bytes.len() + n > cap {
                    let remaining = cap.saturating_sub(bytes.len());
                    bytes.try_reserve(remaining).map_err(|e| e.to_string())?;
                    bytes.extend(buf.get(..remaining).ok_or("invalid capped read")?);
                    return Err("child output limit exceeded".into());
                }
                bytes.try_reserve(n).map_err(|e| e.to_string())?;
                bytes.extend(buf.get(..n).ok_or("invalid child read count")?);
                Ok(false)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            Err(e) => Err(format!("child output failed: {e}")),
        }
    }
    pub struct Output {
        pub status: Option<ExitStatus>,
        pub error: Option<String>,
        pub stdout: Vec<u8>,
        pub stderr: Vec<u8>,
    }
    pub fn bounded(
        mut command: Command,
        input: Vec<u8>,
        deadline: Duration,
    ) -> Result<Output, String> {
        bounded_with(&mut command, input, deadline, |_| {})
    }
    fn bounded_with(
        command: &mut Command,
        input: Vec<u8>,
        deadline: Duration,
        spawned: impl FnOnce(u32),
    ) -> Result<Output, String> {
        bounded_observed(command, input, deadline, spawned, |_| {})
    }
    fn bounded_observed(
        command: &mut Command,
        input: Vec<u8>,
        deadline: Duration,
        spawned: impl FnOnce(u32),
        before_signal: impl FnOnce(u32),
    ) -> Result<Output, String> {
        if input.len() > INPUT {
            return Err("child input limit exceeded".into());
        }
        let start = Instant::now();
        command
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = Owned(
            command
                .spawn()
                .map_err(|e| format!("child spawn failed: {e}"))?,
            false,
        );
        spawned(child.0.id());
        let initialized = (|| {
            let stdin = child.0.stdin.take().ok_or("child stdin unavailable")?;
            let stdout = child.0.stdout.take().ok_or("child stdout unavailable")?;
            let stderr = child.0.stderr.take().ok_or("child stderr unavailable")?;
            nonblocking(&stdin)?;
            nonblocking(&stdout)?;
            nonblocking(&stderr)?;
            Ok::<_, String>((stdin, stdout, stderr))
        })();
        let (stdin, mut stdout_pipe, mut stderr_pipe) = match initialized {
            Ok(pipes) => pipes,
            Err(error) => {
                let stopped = child.stop(before_signal);
                let error = match &stopped {
                    Ok(_) => error,
                    Err(reap) => format!("{error}; {reap}"),
                };
                return Ok(Output {
                    status: stopped.ok(),
                    error: Some(error),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }
        };
        let mut stdin = Some(stdin);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut written = 0;
        let mut out_done = false;
        let mut err_done = false;
        let result = (|| loop {
            if start.elapsed() >= deadline {
                return Err("child deadline exceeded".into());
            }
            let previous = (
                written,
                stdout.len(),
                stderr.len(),
                stdin.is_none(),
                out_done,
                err_done,
            );
            if let Some(pipe) = stdin.as_mut() {
                if written == input.len() {
                    stdin = None;
                } else {
                    let remaining = input.get(written..).ok_or("invalid child write count")?;
                    let chunk = remaining
                        .get(..remaining.len().min(4096))
                        .ok_or("invalid child input slice")?;
                    match pipe.write(chunk) {
                        Ok(0) => return Err("child stdin failed: write zero".into()),
                        Ok(n) => written += n,
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                            ) => {}
                        Err(e) => return Err(format!("child stdin failed: {e}")),
                    }
                }
            }
            if !out_done {
                out_done = read_step(&mut stdout_pipe, &mut stdout, OUTPUT)?;
            }
            if !err_done {
                err_done = read_step(&mut stderr_pipe, &mut stderr, STDERR)?;
            }
            if child.exited()? && stdin.is_none() && out_done && err_done {
                return Ok(());
            }
            if previous
                == (
                    written,
                    stdout.len(),
                    stderr.len(),
                    stdin.is_none(),
                    out_done,
                    err_done,
                )
            {
                thread::sleep(
                    Duration::from_millis(2).min(deadline.saturating_sub(start.elapsed())),
                );
            }
        })();
        // Close local pipes before cleanup; remote holders have no bearing on this.
        drop(stdin);
        drop(stdout_pipe);
        drop(stderr_pipe);
        let stopped = child.stop(before_signal);
        let error = match (result, &stopped) {
            (Err(error), Err(reap)) => Some(format!("{error}; {reap}")),
            (Err(error), _) => Some(error),
            (_, Err(error)) => Some(error.clone()),
            _ => None,
        };
        Ok(Output {
            status: stopped.ok(),
            error,
            stdout,
            stderr,
        })
    }
}
