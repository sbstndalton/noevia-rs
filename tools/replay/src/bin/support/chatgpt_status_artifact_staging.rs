//! Restricted current Cargo coordinator staging. Stable caller-owned ancestors
//! are assumed; final NOFOLLOW descriptors do not prove ancestor race immunity.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const CAP: u64 = 128 * 1024 * 1024;
const LIMITS: &str = "current fresh caller-owned Cargo output; stable trusted ancestors assumed; no compiler or ancestor race immunity proof";
fn require(ok: bool, message: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
#[derive(Clone, Debug, PartialEq)]
struct State {
    dev: u64,
    ino: u64,
    bytes: u64,
    nlink: u64,
    mode: u32,
    uid: u32,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}
fn state(file: &fs::File) -> Result<State> {
    let m = file.metadata()?;
    Ok(State {
        dev: m.dev(),
        ino: m.ino(),
        bytes: m.len(),
        nlink: m.nlink(),
        mode: m.mode(),
        uid: m.uid(),
        mtime: m.mtime(),
        mtime_nsec: m.mtime_nsec(),
        ctime: m.ctime(),
        ctime_nsec: m.ctime_nsec(),
    })
}
#[derive(Clone, Debug)]
struct Image {
    path: PathBuf,
    before: State,
    after: State,
    sha256: String,
}
#[derive(Clone, Debug)]
struct Manifest {
    version: u32,
    kind: String,
    job: String,
    source_head: String,
    source_tree: String,
    compiled_map_sha256: String,
    cargo_root: PathBuf,
    source_root: PathBuf,
    cargo_json: PathBuf,
    cargo_json_sha256: String,
    selected_row: Value,
    selected_row_sha256: String,
    original: Image,
    staged: Image,
    literal_equal: bool,
    cap_bytes: u64,
    limits: String,
}
fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
    match value.get(key) {
        Some(value) => value,
        None => &Value::Null,
    }
}
fn keys(v: &Value, expected: &[&str]) -> Result<()> {
    let o = v.as_object().ok_or("staging object")?;
    require(
        o.len() == expected.len() && expected.iter().all(|k| o.contains_key(*k)),
        "exact staging schema keys",
    )
}
fn string(v: &Value, k: &str) -> Result<String> {
    Ok(field(v, k).as_str().ok_or("staging string")?.into())
}
fn unsigned(v: &Value, k: &str) -> Result<u64> {
    field(v, k)
        .as_u64()
        .ok_or_else(|| "staging unsigned integer".into())
}
fn signed(v: &Value, k: &str) -> Result<i64> {
    field(v, k)
        .as_i64()
        .ok_or_else(|| "staging signed integer".into())
}
impl State {
    fn value(&self) -> Value {
        json!({"dev":self.dev,"ino":self.ino,"bytes":self.bytes,"nlink":self.nlink,"mode":self.mode,"uid":self.uid,"mtime":self.mtime,"mtime_nsec":self.mtime_nsec,"ctime":self.ctime,"ctime_nsec":self.ctime_nsec})
    }
    fn parse(v: &Value) -> Result<Self> {
        keys(
            v,
            &[
                "dev",
                "ino",
                "bytes",
                "nlink",
                "mode",
                "uid",
                "mtime",
                "mtime_nsec",
                "ctime",
                "ctime_nsec",
            ],
        )?;
        Ok(Self {
            dev: unsigned(v, "dev")?,
            ino: unsigned(v, "ino")?,
            bytes: unsigned(v, "bytes")?,
            nlink: unsigned(v, "nlink")?,
            mode: u32::try_from(unsigned(v, "mode")?)?,
            uid: u32::try_from(unsigned(v, "uid")?)?,
            mtime: signed(v, "mtime")?,
            mtime_nsec: signed(v, "mtime_nsec")?,
            ctime: signed(v, "ctime")?,
            ctime_nsec: signed(v, "ctime_nsec")?,
        })
    }
}
impl Image {
    fn value(&self) -> Value {
        json!({"path":self.path,"before":self.before.value(),"after":self.after.value(),"sha256":self.sha256})
    }
    fn parse(v: &Value) -> Result<Self> {
        keys(v, &["path", "before", "after", "sha256"])?;
        Ok(Self {
            path: string(v, "path")?.into(),
            before: State::parse(field(v, "before"))?,
            after: State::parse(field(v, "after"))?,
            sha256: string(v, "sha256")?,
        })
    }
}
impl Manifest {
    fn value(&self) -> Value {
        json!({"version":self.version,"kind":self.kind,"job":self.job,"source_head":self.source_head,"source_tree":self.source_tree,"compiled_map_sha256":self.compiled_map_sha256,"cargo_root":self.cargo_root,"source_root":self.source_root,"cargo_json":self.cargo_json,"cargo_json_sha256":self.cargo_json_sha256,"selected_row":self.selected_row,"selected_row_sha256":self.selected_row_sha256,"original":self.original.value(),"staged":self.staged.value(),"literal_equal":self.literal_equal,"cap_bytes":self.cap_bytes,"limits":self.limits})
    }
    fn parse(v: &Value) -> Result<Self> {
        keys(
            v,
            &[
                "version",
                "kind",
                "job",
                "source_head",
                "source_tree",
                "compiled_map_sha256",
                "cargo_root",
                "source_root",
                "cargo_json",
                "cargo_json_sha256",
                "selected_row",
                "selected_row_sha256",
                "original",
                "staged",
                "literal_equal",
                "cap_bytes",
                "limits",
            ],
        )?;
        Ok(Self {
            version: u32::try_from(unsigned(v, "version")?)?,
            kind: string(v, "kind")?,
            job: string(v, "job")?,
            source_head: string(v, "source_head")?,
            source_tree: string(v, "source_tree")?,
            compiled_map_sha256: string(v, "compiled_map_sha256")?,
            cargo_root: string(v, "cargo_root")?.into(),
            source_root: string(v, "source_root")?.into(),
            cargo_json: string(v, "cargo_json")?.into(),
            cargo_json_sha256: string(v, "cargo_json_sha256")?,
            selected_row: field(v, "selected_row").clone(),
            selected_row_sha256: string(v, "selected_row_sha256")?,
            original: Image::parse(field(v, "original"))?,
            staged: Image::parse(field(v, "staged"))?,
            literal_equal: field(v, "literal_equal")
                .as_bool()
                .ok_or("staging literal boolean")?,
            cap_bytes: unsigned(v, "cap_bytes")?,
            limits: string(v, "limits")?,
        })
    }
}
fn open(path: &Path) -> Result<fs::File> {
    Ok(fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC)
                .bits() as i32,
        )
        .open(path)?)
}
// Fixed kernel-owned process-image link, deliberately followed only here. The
// selected compiler path itself is always opened NOFOLLOW above.
fn process_image() -> Result<fs::File> {
    require(cfg!(target_os = "linux"), "Linux current process image")?;
    Ok(fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC).bits() as i32)
        .open("/proc/self/exe")?)
}
#[allow(dead_code)] // Only the coordinator validates its own mapped image.
pub fn executing(value: &Value) -> Result<()> {
    let image = Image::parse(field(value, "staged"))?;
    image_class(&image.before, true)?;
    require(
        fs::canonicalize(std::env::current_exe()?)? == image.path,
        "current staged executable path",
    )?;
    let mut file = process_image()?;
    require(
        state(&file)? == image.before
            && digest(&mut file, Instant::now() + Duration::from_secs(30))?
                == (image.before.bytes, image.sha256)
            && state(&file)? == image.after,
        "actual mapped staged process image",
    )
}
fn canonical(path: &Path) -> Result<()> {
    require(
        path.is_absolute()
            && path.components().all(|c| {
                matches!(
                    c,
                    std::path::Component::RootDir | std::path::Component::Normal(_)
                )
            })
            && fs::canonicalize(path)? == path,
        "canonical staging input",
    )
}
fn image_class(s: &State, singleton: bool) -> Result<()> {
    require(
        s.mode == 0o100700
            && s.bytes > 0
            && s.bytes <= CAP
            && s.nlink > 0
            && (!singleton || s.nlink == 1),
        "fixed coordinator staging file class",
    )
}
fn digest(file: &mut fs::File, deadline: Instant) -> Result<(u64, String)> {
    file.seek(SeekFrom::Start(0))?;
    let mut sum = Sha256::new();
    let mut count = 0u64;
    let mut buf = [0u8; 8192];
    loop {
        require(Instant::now() < deadline, "staging cooperative deadline")?;
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        count = count.checked_add(n as u64).ok_or("staging overflow")?;
        require(count <= CAP, "staging unchanged cap")?;
        sum.update(buf.get(..n).ok_or("bounded staging hash chunk")?);
    }
    Ok((count, format!("{:x}", sum.finalize())))
}
pub fn select(bytes: &[u8], kind: &str) -> Result<(PathBuf, Value)> {
    require(bytes.len() <= 8 * 1024 * 1024, "bounded Cargo JSON")?;
    let (name, target, test) = match kind {
        "tool" => ("chatgpt-status-contracts", "bin", false),
        "library-test" => ("noevia_server", "lib", true),
        _ => return Err("fixed qualification artifact required".into()),
    };
    let mut candidate = None;
    let mut finished = 0;
    for line in std::str::from_utf8(bytes)?.lines() {
        let row: Value = serde_json::from_str(line)?;
        if field(&row, "reason") == "build-finished" {
            require(field(&row, "success") == true, "actual Cargo success")?;
            finished += 1;
        }
        if field(&row, "reason") == "compiler-artifact"
            && field(field(&row, "target"), "name") == name
            && field(field(&row, "target"), "kind") == &serde_json::json!([target])
            && field(field(&row, "profile"), "test") == test
            && row.get("executable").is_some_and(|v| !v.is_null())
        {
            require(candidate.is_none(), "unique qualification artifact")?;
            candidate = Some(row);
        }
    }
    require(finished == 1, "one actual Cargo completion")?;
    let row = candidate.ok_or("qualification artifact missing")?;
    let path = PathBuf::from(field(&row, "executable").as_str().ok_or("artifact path")?);
    canonical(&path)?;
    require(path.is_file(), "canonical actual artifact")?;
    Ok((path, row))
}
fn selected(row: &Value, original: &Path, source: &Path) -> Result<()> {
    require(
        field(row, "reason") == "compiler-artifact"
            && field(field(row, "target"), "name") == "chatgpt-status-contracts"
            && field(field(row, "target"), "kind") == &serde_json::json!(["bin"])
            && field(field(row, "profile"), "test") == false
            && field(row, "executable").as_str() == original.to_str()
            && field(row, "manifest_path").as_str()
                == source.join("tools/replay/Cargo.toml").to_str()
            && field(row, "package_id").as_str()
                == Some(
                    format!(
                        "path+file://{}#0.1.0",
                        source.join("tools/replay").display()
                    )
                    .as_str(),
                )
            && field(row, "fresh") == false
            && field(field(row, "target"), "src_path").as_str()
                == source
                    .join("tools/replay/src/bin/chatgpt-status-contracts.rs")
                    .to_str()
            && field(row, "profile")
                == &json!({"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false}),
        "current selected coordinator target manifest profile",
    )
}
fn private_ancestry(path: &Path, uid: u32) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    require(
        m.is_dir() && m.uid() == uid && m.mode() & 0o7777 == 0o700,
        "owned private staging ancestor",
    )
}
fn cargo_ancestry(original: &Path, root: &Path, uid: u32) -> Result<()> {
    canonical(root)?;
    require(
        original == root.join("debug/chatgpt-status-contracts"),
        "fixed original Cargo output",
    )?;
    let mut p = original.parent().ok_or("Cargo parent")?;
    loop {
        private_ancestry(p, uid)?;
        if p == root {
            break;
        }
        p = p.parent().ok_or("Cargo ancestry")?;
    }
    Ok(())
}
#[allow(dead_code)] // The integration reader compiles this module but never stages.
pub fn stage(
    cargo_json: &Path,
    cargo_root: &Path,
    source: &Path,
    root: &Path,
    identity: &Value,
    job: &str,
    map: &Value,
) -> Result<PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(30);
    canonical(cargo_json)?;
    canonical(source)?;
    canonical(root)?;
    let mut json_file = open(cargo_json)?;
    let json_before = state(&json_file)?;
    require(
        json_before.mode == 0o100600
            && json_before.nlink == 1
            && json_before.bytes <= 8 * 1024 * 1024,
        "private current Cargo JSON",
    )?;
    let mut raw = Vec::new();
    Read::by_ref(&mut json_file)
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut raw)?;
    require(
        state(&json_file)? == json_before && raw.len() as u64 == json_before.bytes,
        "stable Cargo JSON",
    )?;
    let (original, row) = select(&raw, "tool")?;
    selected(&row, &original, source)?;
    let uid = fs::symlink_metadata(source)?.uid();
    require(json_before.uid == uid, "owned Cargo JSON")?;
    require(
        uid == rustix::process::geteuid().as_raw()
            && std::env::var("NOEVIA_WORKER_AUTH_SOURCE_ROOT")?.as_str()
                == source.to_str().ok_or("source root utf8")?,
        "actual owner source root",
    )?;
    private_ancestry(root, uid)?;
    cargo_ancestry(&original, cargo_root, uid)?;
    require(
        !root.starts_with(source)
            && !source.starts_with(root)
            && !root.starts_with(cargo_root)
            && !cargo_root.starts_with(root),
        "staging roots disjoint",
    )?;
    let mut input = open(&original)?;
    let before = state(&input)?;
    image_class(&before, false)?;
    require(before.uid == uid, "owned original coordinator")?;
    let bootstrap = fs::canonicalize(std::env::current_exe()?)?;
    let boot = process_image()?;
    require(
        bootstrap == original && state(&boot)? == before,
        "actual selected bootstrap descriptor",
    )?;
    let staged = root.join("coordinator");
    let mut output = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o700)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC).bits() as i32)
        .open(&staged)?;
    let mut sum = Sha256::new();
    let mut bytes = 0u64;
    let mut buf = [0u8; 8192];
    loop {
        require(Instant::now() < deadline, "staging cooperative deadline")?;
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes = bytes.checked_add(n as u64).ok_or("staging overflow")?;
        require(bytes <= CAP, "staging unchanged cap")?;
        output.write_all(buf.get(..n).ok_or("bounded staging write chunk")?)?;
        sum.update(buf.get(..n).ok_or("bounded staging hash chunk")?);
    }
    output.flush()?;
    let sha = format!("{:x}", sum.finalize());
    let after = state(&input)?;
    require(
        before == after && bytes == before.bytes,
        "original same-FD stable copy",
    )?;
    let staged_before = state(&output)?;
    image_class(&staged_before, true)?;
    require(staged_before.uid == uid, "owned staged coordinator")?;
    input.seek(SeekFrom::Start(0))?;
    output.seek(SeekFrom::Start(0))?;
    let mut other = [0u8; 8192];
    let mut remaining = bytes;
    while remaining > 0 {
        require(Instant::now() < deadline, "staging literal deadline")?;
        let n = usize::try_from(remaining.min(8192))?;
        input.read_exact(buf.get_mut(..n).ok_or("bounded staging literal input")?)?;
        output.read_exact(other.get_mut(..n).ok_or("bounded staging literal output")?)?;
        require(
            buf.get(..n).ok_or("bounded original literal")?
                == other.get(..n).ok_or("bounded staged literal")?,
            "literal original staged equality",
        )?;
        remaining -= n as u64;
    }
    require(
        digest(&mut input, deadline)? == (bytes, sha.clone())
            && digest(&mut output, deadline)? == (bytes, sha.clone())
            && state(&input)? == before
            && state(&output)? == staged_before,
        "independent reread metadata hash equality",
    )?;
    let manifest = Manifest {
        version: 1,
        kind: "current-cargo-coordinator-singleton".into(),
        job: job.into(),
        source_head: field(identity, "head")
            .as_str()
            .ok_or("source head")?
            .into(),
        source_tree: field(identity, "tree")
            .as_str()
            .ok_or("source tree")?
            .into(),
        compiled_map_sha256: hash(&serde_json::to_vec(map)?),
        cargo_root: cargo_root.into(),
        source_root: source.into(),
        cargo_json: cargo_json.into(),
        cargo_json_sha256: hash(&raw),
        selected_row_sha256: hash(&serde_json::to_vec(&row)?),
        selected_row: row,
        original: Image {
            path: original,
            before: before.clone(),
            after: before,
            sha256: sha.clone(),
        },
        staged: Image {
            path: staged,
            before: staged_before.clone(),
            after: staged_before,
            sha256: sha,
        },
        literal_equal: true,
        cap_bytes: CAP,
        limits: LIMITS.into(),
    };
    let bytes = serde_json::to_vec_pretty(&manifest.value())?;
    require(bytes.len() <= 64 * 1024, "staging manifest cap")?;
    let path = root.join("coordinator-staging.json");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC).bits() as i32)
        .open(&path)?;
    file.write_all(&bytes)?;
    Ok(path)
}
pub fn validate(bytes: &[u8], identity: &Value, job: &str, map: &Value) -> Result<Value> {
    require(bytes.len() <= 64 * 1024, "staging manifest cap")?;
    let value: Value = serde_json::from_slice(bytes)?;
    require(
        serde_json::to_vec_pretty(&value)? == bytes,
        "canonical duplicate-free staging schema",
    )?;
    let m = Manifest::parse(&value)?;
    require(
        m.version == 1
            && m.kind == "current-cargo-coordinator-singleton"
            && m.job == job
            && Some(m.source_head.as_str()) == field(identity, "head").as_str()
            && Some(m.source_tree.as_str()) == field(identity, "tree").as_str()
            && m.compiled_map_sha256 == hash(&serde_json::to_vec(map)?)
            && m.cap_bytes == CAP
            && m.limits == LIMITS
            && m.literal_equal
            && std::env::var("NOEVIA_WORKER_AUTH_SOURCE_ROOT")?.as_str()
                == m.source_root.to_str().ok_or("source root utf8")?
            && m.original.before.uid == rustix::process::geteuid().as_raw(),
        "current full staging schema source job",
    )?;
    selected(&m.selected_row, &m.original.path, &m.source_root)?;
    require(
        m.selected_row_sha256 == hash(&serde_json::to_vec(&m.selected_row)?)
            && m.original.before == m.original.after
            && m.staged.before == m.staged.after
            && m.original.before.uid == m.staged.before.uid
            && m.original.sha256 == m.staged.sha256
            && m.original.before.bytes == m.staged.before.bytes
            && (m.original.before.dev, m.original.before.ino)
                != (m.staged.before.dev, m.staged.before.ino),
        "distinct original staged stable identity",
    )?;
    image_class(&m.original.before, false)?;
    image_class(&m.staged.before, true)?;
    cargo_ancestry(&m.original.path, &m.cargo_root, m.original.before.uid)?;
    canonical(&m.source_root)?;
    let root = m.staged.path.parent().ok_or("staged parent")?;
    private_ancestry(root, m.original.before.uid)?;
    require(
        !root.starts_with(&m.source_root)
            && !m.source_root.starts_with(root)
            && !root.starts_with(&m.cargo_root)
            && !m.cargo_root.starts_with(root),
        "validated disjoint staging roots",
    )?;
    require(
        m.staged
            .path
            .file_name()
            .is_some_and(|v| v == "coordinator"),
        "fixed staged image",
    )?;
    let deadline = Instant::now() + Duration::from_secs(30);
    for image in [&m.original, &m.staged] {
        canonical(&image.path)?;
        let mut file = open(&image.path)?;
        require(
            state(&file)? == image.before
                && digest(&mut file, deadline)? == (image.before.bytes, image.sha256.clone())
                && state(&file)? == image.after,
            "current original staged descriptor binding",
        )?;
    }
    let mut original = open(&m.original.path)?;
    let mut staged = open(&m.staged.path)?;
    require(
        state(&original)? == m.original.before && state(&staged)? == m.staged.before,
        "literal validation descriptors",
    )?;
    let mut a = [0u8; 8192];
    let mut b = [0u8; 8192];
    let mut left = m.original.before.bytes;
    while left > 0 {
        require(
            Instant::now() < deadline,
            "staging literal validation deadline",
        )?;
        let n = usize::try_from(left.min(8192))?;
        original.read_exact(a.get_mut(..n).ok_or("bounded original verification")?)?;
        staged.read_exact(b.get_mut(..n).ok_or("bounded staged verification")?)?;
        require(
            a.get(..n).ok_or("bounded original equality")?
                == b.get(..n).ok_or("bounded staged equality")?,
            "current literal staged equality",
        )?;
        left -= n as u64;
    }
    require(
        state(&original)? == m.original.after && state(&staged)? == m.staged.after,
        "literal validation stable metadata",
    )?;
    let mut f = open(&m.cargo_json)?;
    let before = state(&f)?;
    require(
        before.mode == 0o100600
            && before.uid == m.original.before.uid
            && before.nlink == 1
            && before.bytes <= 8 * 1024 * 1024,
        "private selected Cargo JSON",
    )?;
    let mut raw = Vec::new();
    Read::by_ref(&mut f)
        .take(8 * 1024 * 1024 + 1)
        .read_to_end(&mut raw)?;
    require(
        state(&f)? == before
            && raw.len() as u64 == before.bytes
            && hash(&raw) == m.cargo_json_sha256
            && select(&raw, "tool")? == (m.original.path.clone(), m.selected_row.clone()),
        "current original Cargo selection",
    )?;
    Ok(m.value())
}
#[cfg(test)]
mod controls {
    use super::*;
    #[test]
    fn missing_manifest_and_guard_classes_refused() {
        assert!(validate(b"{}", &Value::Null, "job", &Value::Null).is_err());
        let s = State {
            dev: 1,
            ino: 2,
            bytes: 1,
            nlink: 2,
            mode: 0o100700,
            uid: 1,
            mtime: 1,
            mtime_nsec: 0,
            ctime: 1,
            ctime_nsec: 0,
        };
        assert!(image_class(&s, false).is_ok());
        assert!(image_class(&s, true).is_err());
        let mut bad = s.clone();
        bad.bytes = CAP + 1;
        assert!(image_class(&bad, false).is_err());
        bad = s;
        bad.mode = 0o040700;
        assert!(image_class(&bad, false).is_err());
        assert!(select(b"{\"reason\":\"build-finished\",\"success\":false}", "tool").is_err());
    }
    #[test]
    fn exact_selected_profile_manifest_and_schema_required() -> Result<()> {
        let source = Path::new("/owned/source");
        let original = Path::new("/owned/target/debug/chatgpt-status-contracts");
        let row = json!({"reason":"compiler-artifact","package_id":"path+file:///owned/source/tools/replay#0.1.0","manifest_path":"/owned/source/tools/replay/Cargo.toml","executable":original,"fresh":false,"target":{"name":"chatgpt-status-contracts","kind":["bin"],"src_path":"/owned/source/tools/replay/src/bin/chatgpt-status-contracts.rs"},"profile":{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false}});
        selected(&row, original, source)?;
        for key in [
            "manifest_path",
            "package_id",
            "profile",
            "target",
            "executable",
            "fresh",
        ] {
            let mut changed = row.clone();
            changed
                .as_object_mut()
                .ok_or("control row")?
                .insert(key.into(), Value::Null);
            assert!(selected(&changed, original, source).is_err());
        }
        assert!(Manifest::parse(&json!({"version":1})).is_err());
        assert!(Image::parse(
            &json!({"path":"/owned/target","before":{},"after":{},"sha256":"x","extra":true})
        )
        .is_err());
        assert!(keys(&json!({"expected":true,"extra":true}), &["expected"]).is_err());
        Ok(())
    }
}
