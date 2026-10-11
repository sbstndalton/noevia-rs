//! Private fixed qualification only. Logical caps assume stable trusted ancestors.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};
pub type Result<T> = std::result::Result<T, String>;
pub const CORE: &str = "010735c7d86d30269ab3f4a351b2135faea1c8e7";
pub const TREE: &str = "345adfa12da083eb5888ec232dc2d86b9b0383a5";
pub const CANDIDATE: &str = "b375290c638f9f033ece2335ec2be35dfca86e35";
pub const ROOT: &str = "/tmp/m4-storage-factory-oracle-b375-stage-b";
pub const GIT: &str = "/Users/sebastiandalton/noevia-work/m6-chatgpt-status-core";
pub const DEPS: &str = "/tmp/s1-diagnose-node-dependencies";
pub const PROOF: &str = "/tmp/s1-diagnose-dependency-copy.json";
pub const PROOF_SHA: &str = "98e91b95e8812e863a70dce0fe66bf39c4507b7e50b660936d244d509ad819cc";
pub const NODE: &str = "/opt/homebrew/opt/node@22/bin/node";
pub const NODE_SHA: &str = "b6a0496318476702af424b12fe5f245a74c3cb93159e0c7411e6a95ad8c8d4db";
// Fixed installed native error dialect grounding, not complete dylib ancestry.
pub const NODE_CRYPTO_LIBRARY: &str =
    "/opt/homebrew/Cellar/node@22/22.23.3_1/lib/libnode.127.dylib";
pub const NODE_CRYPTO_LIBRARY_SHA: &str =
    "cdceec4c9d2a50079954ade5a89f4e7887a620c0c4329296b1af660926a66f65";
pub const JS_GCM_AUTH_FAILURE: &str = "Unsupported state or unable to authenticate data";
pub const STORAGE_EVIDENCE_SQL: &str = "SELECT * FROM storage_connections WHERE user_id=?";
pub const WASM_SOURCE: &str = "/tmp/m4-storage-factory-approved-wasm";
pub const WASM_SHA: &str = "040738a438de8241bd2cd1558f2b06beaa32c5c9be01c30f09cd87cb2e978575";
pub const NOW: i64 = 1_700_000_000_000;
// Root must replace these only after inspecting the exact retained stage bytes.
// Empty means unapproved; neither the CLI nor the tool can approve itself.
pub const APPROVED_MANIFEST_SHA: &str = "";
pub const APPROVED_PREFLIGHT_SHA: &str = "";
pub const GIT_EVIDENCE: &str = "/tmp/m4-storage-factory-git-second-attempt";
pub const CAPTURE_ROOT: &str = "/tmp/m4-storage-factory-capture-first-attempt";
pub const PREFLIGHT_ROOT: &str = "/tmp/m4-storage-factory-preflight-first-attempt";
pub const SCHEMA: &str = include_str!("../../../server-store/tests/fixtures/node-schema.sql");
pub fn require(ok: bool, message: &str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}
pub fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect()
}
pub fn read(p: &Path, cap: usize) -> Result<Vec<u8>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC)
                .bits() as i32,
        )
        .open(p)
        .map_err(|_| "fixed input open")?;
    let m = file.metadata().map_err(|_| "fixed input stat")?;
    require(
        m.is_file() && m.nlink() == 1 && m.len() <= cap as u64,
        "regular singleton cap",
    )?;
    let mut b = Vec::new();
    file.take(cap as u64 + 1)
        .read_to_end(&mut b)
        .map_err(|_| "fixed input read")?;
    require(b.len() <= cap, "read cap")?;
    Ok(b)
}
pub fn directory(p: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(p)
        .map_err(|_| "fresh directory required".into())
}
pub fn put(p: &Path, b: &[u8]) -> Result<()> {
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC).bits() as i32)
        .open(p)
        .map_err(|_| "fresh evidence required")?;
    f.write_all(b).map_err(|_| "evidence write".into())
}
pub fn relative(s: &str) -> Result<PathBuf> {
    let p = PathBuf::from(s);
    require(
        !s.is_empty()
            && s.split('/').all(|part| {
                !part.is_empty() && part != "." && part != ".." && !part.contains('\0')
            })
            && s.len() <= 4096
            && !p.is_absolute()
            && p.components().all(|v| matches!(v, Component::Normal(_))),
        "relative path",
    )?;
    Ok(p)
}
fn push<T>(v: &mut Vec<T>, value: T, cap: usize) -> Result<()> {
    require(v.len() < cap, "preappend cap")?;
    v.try_reserve(1).map_err(|_| "allocation")?;
    v.push(value);
    Ok(())
}
pub fn contained_module(root: &Path, name: &str, files: &BTreeSet<PathBuf>) -> bool {
    let p = Path::new(name);
    p.is_absolute()
        && !name.contains("//")
        && !name.split('/').any(|part| part == "." || part == "..")
        && p.components()
            .all(|v| matches!(v, Component::RootDir | Component::Normal(_)))
        && p.strip_prefix(root).ok().is_some_and(|r| {
            files.contains(r) && root.join(r).as_os_str() == std::ffi::OsStr::new(name)
        })
}
pub fn json_bytes(v: &Value, cap: usize) -> Result<Vec<u8>> {
    struct Limit(Vec<u8>, usize);
    impl Write for Limit {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if b.len() > self.1.saturating_sub(self.0.len()) {
                return Err(std::io::Error::other("JSON cap"));
            }
            self.0.try_reserve(b.len()).map_err(std::io::Error::other)?;
            self.0.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = Limit(Vec::new(), cap);
    serde_json::to_writer(&mut out, v).map_err(|_| "JSON cap")?;
    Ok(out.0)
}
pub fn text(s: &str) -> Value {
    json!({"t":"text","v":s})
}
pub fn row() -> Value {
    json!({"user_id":text("a"),"kind":text("webdav"),"base_url":text("https://storage.invalid/dav"),"bucket":text("synthetic"),"region":text("us-east-1"),"username":text("synthetic"),"secret":text("synthetic-secret"),"corpus_root":text("synthetic"),"updated_at":{"t":"integer","v":NOW}})
}
pub fn set_row(case: &mut Value, field: &str, value: Value) -> Result<()> {
    let slot = case
        .get_mut("row")
        .and_then(|r| r.get_mut(field))
        .ok_or("fixed field")?;
    *slot = value;
    Ok(())
}
fn base(id: &str, lane: &str) -> Value {
    json!({"id":id,"lane":lane,"owner":"a","row":row(),"current":hex(&[7;32]),"previous":null,"previous_env":null,"previous_directory":false,"shape":"","headers":{},"method":"GET","path":"/api/integrations/storage","device_enabled":true,"public":false})
}
fn sealed(key: u8, plain: &[u8], owner: Option<&[u8]>) -> Result<String> {
    secret_envelope::seal(&[key; 32], &[6; 12], plain, owner).map_err(|_| "synthetic seal".into())
}
pub fn cases() -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut missing = base("F-missing", "functional");
    missing.get_mut("row").ok_or("row")?.take();
    push(&mut out, missing, 126)?;
    for kind in ["local", "nextcloud", "webdav", "s3", "unknown-text"] {
        let mut c = base(&format!("F-kind-{kind}"), "functional");
        set_row(&mut c, "kind", text(kind))?;
        *c.get_mut("public").ok_or("public")? = json!(true);
        *c.get_mut("headers").ok_or("headers")? =
            json!({"cookie":"cowork_session=synthetic-session-a; cowork_csrf=synthetic-csrf-a"});
        push(&mut out, c, 126)?;
    }
    let cv1 = sealed(7, b"synthetic-secret", None)?;
    let cv2 = sealed(7, b"synthetic-secret", Some(b"a"))?;
    let mut raw = secret_envelope::decode_base64_node(
        &cv2.get(7..)
            .ok_or("prefix")?
            .encode_utf16()
            .collect::<Vec<_>>(),
    );
    let tail = raw.last_mut().ok_or("ciphertext")?;
    *tail ^= 1;
    let vals = [
        ("empty", "".into()),
        ("plain", "synthetic-secret".into()),
        ("v3", "enc:v3:synthetic".into()),
        ("current-v1", cv1.clone()),
        ("current-v2", cv2.clone()),
        ("previous-v1", sealed(8, b"synthetic-secret", None)?),
        ("previous-v2", sealed(8, b"synthetic-secret", Some(b"a"))?),
        ("crossowner", sealed(7, b"synthetic-secret", Some(b"b"))?),
        ("short", "enc:v2:QQ".into()),
        (
            "tag",
            format!("enc:v2:{}", secret_envelope::encode_base64url(&raw)),
        ),
        ("wrongkey", sealed(9, b"synthetic-secret", Some(b"a"))?),
        ("prefix", cv1.replacen("enc:v1:", "enc:v2:", 1)),
        ("empty-v1", sealed(7, b"", None)?),
        ("empty-v2", sealed(7, b"", Some(b"a"))?),
        ("utf8", sealed(7, &[0xff, 0xe2, 0x82], Some(b"a"))?),
        (
            "lenient",
            format!(
                "{}!{}",
                cv2.get(..9).ok_or("prefix")?,
                cv2.get(9..).ok_or("suffix")?
            ),
        ),
    ];
    for (name, value) in vals {
        let mut c = base(&format!("F-secret-{name}"), "functional");
        if name.starts_with("previous") {
            *c.get_mut("previous").ok_or("previous")? = json!(hex(&[8; 32]));
        }
        set_row(&mut c, "secret", text(&value))?;
        push(&mut out, c, 126)?;
    }
    for (i, region) in [
        "",
        "us-east-1",
        "eu-west-1",
        " EU-WEST-1 ",
        "\u{feff}\u{a0}EU-WEST-1\u{a0}",
        "bad_region",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "é",
    ]
    .into_iter()
    .enumerate()
    {
        let mut c = base(&format!("F-region-{i}"), "functional");
        set_row(&mut c, "region", text(region))?;
        push(&mut out, c, 126)?;
    }
    for name in [
        "missing",
        "current31",
        "current33",
        "previous-missing",
        "previous31",
        "previous33",
        "equal",
        "hex-env",
        "base64-env",
        "space-env",
        "file-precedence",
        "previous-directory",
    ] {
        let mut c = base(&format!("K-{name}"), "key");
        match name {
            "missing" => {
                *c.get_mut("current").ok_or("key")? = Value::Null;
            }
            "current31" | "current33" => {
                *c.get_mut("current").ok_or("key")? =
                    json!(hex(&vec![7; if name == "current31" { 31 } else { 33 }]));
            }
            "previous31" | "previous33" => {
                *c.get_mut("previous").ok_or("key")? =
                    json!(hex(&vec![8; if name == "previous31" { 31 } else { 33 }]));
            }
            "equal" => {
                *c.get_mut("previous").ok_or("key")? = json!(hex(&[7; 32]));
            }
            "hex-env" => {
                *c.get_mut("previous_env").ok_or("env")? = json!(hex(&[8; 32]));
            }
            "base64-env" => {
                *c.get_mut("previous_env").ok_or("env")? =
                    json!(secret_envelope::encode_base64url(&[8; 32]));
            }
            "space-env" => {
                *c.get_mut("previous_env").ok_or("env")? = json!(" \n");
            }
            "file-precedence" => {
                *c.get_mut("previous").ok_or("key")? = json!(hex(&[8; 32]));
                *c.get_mut("previous_env").ok_or("env")? = json!("bad");
            }
            "previous-directory" => {
                *c.get_mut("previous_directory").ok_or("dir")? = json!(true);
            }
            _ => {}
        }
        push(&mut out, c, 126)?;
    }
    for (field, cap) in [
        ("user_id", 65536),
        ("kind", 32),
        ("base_url", 65536),
        ("bucket", 65536),
        ("region", 65536),
        ("username", 65536),
        ("secret", 131072),
        ("corpus_root", 65536),
    ] {
        for n in [cap - 1, cap, cap + 1] {
            let mut c = base(&format!("C-{field}-{n}"), "cap");
            let v = if field == "secret" {
                let mut s = cv2.clone();
                require(s.len() <= n, "padding")?;
                s.push_str(&"!".repeat(n - s.len()));
                s
            } else {
                "a".repeat(n)
            };
            set_row(&mut c, field, text(&v))?;
            if field == "user_id" {
                *c.get_mut("owner").ok_or("owner")? = json!(v);
                set_row(&mut c, "secret", text("synthetic"))?;
            }
            push(&mut out, c, 126)?;
        }
    }
    let mut empty = base("C-owner-empty", "cap");
    *empty.get_mut("owner").ok_or("owner")? = json!("");
    set_row(&mut empty, "user_id", text(""))?;
    push(&mut out, empty, 126)?;
    for n in [65535, 65536, 65537] {
        let mut c = base(&format!("C-opened-{n}"), "cap");
        set_row(
            &mut c,
            "secret",
            text(&sealed(7, &vec![b'a'; n], Some(b"a"))?),
        )?;
        push(&mut out, c, 126)?;
    }
    for n in [262143, 262144, 262145] {
        let mut c = base(&format!("C-aggregate-{n}"), "cap");
        let base_len = 1 + 6 + 9 + 9;
        let remain = n - base_len;
        set_row(&mut c, "kind", text("webdav"))?;
        set_row(&mut c, "secret", text("synthetic"))?;
        set_row(&mut c, "region", text("us-east-1"))?;
        let mut left = remain;
        for field in ["base_url", "bucket", "username", "corpus_root"] {
            let take = left.min(65536);
            set_row(&mut c, field, text(&"a".repeat(take)))?;
            left -= take;
        }
        require(left == 0, "aggregate admitted fields")?;
        push(&mut out, c, 126)?;
    }
    for field in [
        "user_id",
        "kind",
        "base_url",
        "bucket",
        "region",
        "username",
        "secret",
        "corpus_root",
    ] {
        for (tag, value) in [
            ("null", Value::Null),
            ("integer", json!(7)),
            ("real", json!(7.5)),
            ("blob", json!("41")),
        ] {
            let mut c = base(&format!("T-{field}-{tag}"), "type");
            set_row(&mut c, field, json!({"t":tag,"v":value}))?;
            *c.get_mut("shape").ok_or("shape")? = json!("loose");
            push(&mut out, c, 126)?;
        }
    }
    for (tag, value) in [
        ("text", json!("7")),
        ("null", Value::Null),
        ("real", json!(7.5)),
        ("blob", json!("41")),
    ] {
        let mut c = base(&format!("T-updated_at-{tag}"), "type");
        set_row(&mut c, "updated_at", json!({"t":tag,"v":value}))?;
        *c.get_mut("shape").ok_or("shape")? = json!("loose");
        push(&mut out, c, 126)?;
    }
    for shape in ["missing", "view", "missing-region", "no-pk", "composite-pk"] {
        let mut c = base(&format!("S-{shape}"), "schema");
        *c.get_mut("shape").ok_or("shape")? = json!(shape);
        push(&mut out, c, 126)?;
    }
    for name in [
        "none",
        "cookie-a",
        "cookie-b",
        "device",
        "mixed",
        "bad-csrf",
        "good-csrf",
        "foreign-origin",
        "expired",
        "disabled",
        "feature-off",
        "malformed-cookie",
    ] {
        let mut c = base(&format!("G-{name}"), "gate");
        let mut h = BTreeMap::new();
        if matches!(
            name,
            "cookie-a"
                | "cookie-b"
                | "mixed"
                | "bad-csrf"
                | "good-csrf"
                | "foreign-origin"
                | "expired"
                | "disabled"
        ) {
            h.insert(
                "cookie",
                "cowork_session=synthetic-session-a; cowork_csrf=synthetic-csrf-a",
            );
        }
        if name == "cookie-b" {
            h.insert("cookie", "cowork_session=synthetic-session-b");
        }
        if matches!(name, "device" | "mixed" | "feature-off") {
            h.insert("authorization", "Bearer nva_synthetic-device-a");
        }
        if name == "malformed-cookie" {
            h.insert("cookie", "cowork_session=%ZZ");
        }
        if matches!(name, "bad-csrf" | "good-csrf" | "foreign-origin") {
            *c.get_mut("method").ok_or("method")? = json!("POST");
            *c.get_mut("path").ok_or("path")? = json!("/api/integrations/storage/file");
            h.insert(
                "origin",
                if name == "foreign-origin" {
                    "https://foreign.invalid"
                } else {
                    "https://owner.invalid"
                },
            );
            h.insert(
                "x-csrf-token",
                if name == "bad-csrf" {
                    "bad"
                } else {
                    "synthetic-csrf-a"
                },
            );
        }
        if name == "feature-off" {
            *c.get_mut("device_enabled").ok_or("enabled")? = json!(false);
        }
        *c.get_mut("headers").ok_or("headers")? = json!(h);
        push(&mut out, c, 126)?;
    }
    require(out.len() == 126, "exact corpus")?;
    let unique: BTreeSet<_> = out
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str))
        .collect();
    require(unique.len() == 126, "unique corpus")?;
    Ok(out)
}
pub fn shape_sql(shape: &str) -> Result<String> {
    if shape.is_empty() {
        return Ok(String::new());
    }
    let mut columns = "user_id,kind,base_url,bucket,region,username,secret,corpus_root,updated_at"
        .split(',')
        .map(|s| format!("{s} {}", if s == "user_id" { "PRIMARY KEY" } else { "" }))
        .collect::<Vec<_>>();
    match shape {"missing"=>return Ok("DROP TABLE storage_connections".into()),"view"=>return Ok("DROP TABLE storage_connections; CREATE VIEW storage_connections AS SELECT 'a' AS user_id".into()),"missing-region"=>columns.retain(|s|!s.starts_with("region ")),"no-pk"=>{for s in &mut columns{*s=s.replace("PRIMARY KEY","");}},"composite-pk"=>{for s in &mut columns{*s=s.replace("PRIMARY KEY","");}columns.push("PRIMARY KEY(user_id,kind)".into());},"loose"=>{},_=>return Err("fixed shape".into())}
    Ok(format!(
        "DROP TABLE storage_connections; CREATE TABLE storage_connections({})",
        columns.join(",")
    ))
}
fn command(args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = Command::new("/usr/bin/git");
    cmd.current_dir(GIT)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .args(args);
    let o = crate::process::bounded(cmd, vec![], Duration::from_secs(15));
    static SEQUENCE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let number = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    require(number < 1026, "Git command count")?;
    let stem = format!("git-{number:04}");
    put(
        &Path::new(GIT_EVIDENCE).join(format!("{stem}.command.json")),
        &json_bytes(
            &json!({"program":"/usr/bin/git","args":args,"cwd":GIT,"env":{"PATH":"/usr/bin:/bin"}}),
            65536,
        )?,
    )?;
    retain_capture(Path::new(GIT_EVIDENCE), &stem, &o)?;
    require(
        o.failure.is_none() && o.status.is_some_and(|s| s.success()) && o.stderr.is_empty(),
        "immutable Git read",
    )?;
    Ok(o.stdout)
}
// The immutable proof names 65 top-level packages, while the immutable lock
// also pins exactly one nested package root. Package identity is path-based.
pub const NESTED_TSLIB_ROOT: &str = "node_modules/tsyringe/node_modules/tslib";
pub fn expected_package_roots(closure: &BTreeSet<String>) -> Result<BTreeSet<String>> {
    require(
        closure.len() == 65 && closure.contains("tslib") && closure.contains("tsyringe"),
        "fixed package closure names",
    )?;
    let mut roots = closure
        .iter()
        .map(|name| format!("node_modules/{name}"))
        .collect::<BTreeSet<_>>();
    require(
        roots.insert(NESTED_TSLIB_ROOT.into()),
        "nested package root",
    )?;
    require(roots.len() == 66, "fixed package root count")?;
    Ok(roots)
}
pub fn validate_package_root(
    parent: &str,
    package: &Value,
    pinned: &Value,
    closure: &BTreeSet<String>,
    expected: &BTreeSet<String>,
    manifests: &mut BTreeSet<String>,
) -> Result<()> {
    require(expected.contains(parent), "unexpected locked package root")?;
    let name = string(package, "name")?;
    let version = string(package, "version")?;
    let identity = if parent == NESTED_TSLIB_ROOT {
        name == "tslib" && version == "1.14.1"
    } else {
        parent == format!("node_modules/{name}") && (name != "tslib" || version == "2.8.1")
    };
    require(
        identity
            && closure.contains(name)
            && pinned.get("version").and_then(Value::as_str) == Some(version),
        "locked package identity/version",
    )?;
    require(
        manifests.insert(parent.into()),
        "duplicate locked package root",
    )
}
pub fn validate_package_root_closure(
    manifests: &BTreeSet<String>,
    expected: &BTreeSet<String>,
) -> Result<()> {
    require(
        manifests == expected && manifests.len() == 66,
        "exact locked package root closure",
    )
}
pub fn materialize(adapter: &str) -> Result<Value> {
    let started = std::time::Instant::now();
    let root = Path::new(ROOT);
    require(!root.exists(), "fresh topology")?;
    let proof_bytes = read(Path::new(PROOF), 2 * 1024 * 1024)?;
    require(hash(&proof_bytes) == PROOF_SHA, "accepted proof")?;
    let proof: Value = serde_json::from_slice(&proof_bytes).map_err(|_| "proof JSON")?;
    require(
        proof.get("destination").and_then(Value::as_str) == Some(DEPS)
            && proof.get("total_bytes").and_then(Value::as_u64) == Some(19_185_089)
            && proof
                .get("closure")
                .and_then(Value::as_array)
                .is_some_and(|v| v.len() == 65),
        "proof identity",
    )?;
    let wasm = read(Path::new(WASM_SOURCE), 16 * 1024 * 1024)?;
    require(hash(&wasm) == WASM_SHA, "pinned accepted WASM required")?;
    let node = read(Path::new(NODE), 128 * 1024 * 1024)?;
    require(hash(&node) == NODE_SHA, "fixed Node binary")?;
    directory(Path::new(GIT_EVIDENCE))?;
    require(
        command(&["rev-parse", &format!("{CORE}^{{tree}}")])? == format!("{TREE}\n").as_bytes(),
        "fixed CORE tree",
    )?;
    let listing = command(&["ls-tree", "-rz", CORE, "--", "server"])?;
    require(listing.len() <= 256 * 1024, "Git list cap")?;
    let mut bytes_total = 0usize;
    let mut material = Vec::new();
    for raw in listing.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        require(
            started.elapsed() < Duration::from_secs(60),
            "materialize cooperative deadline",
        )?;
        let record = std::str::from_utf8(raw).map_err(|_| "Git record")?;
        let (identity, name) = record.split_once('\t').ok_or("Git shape")?;
        if !(name.ends_with(".cjs")
            || matches!(
                name,
                "server/package.json" | "server/package-lock.json" | "server/dav-parse.lock"
            ))
        {
            continue;
        }
        let fields: Vec<_> = identity.split(' ').collect();
        let [mode, kind, blob] = fields.as_slice() else {
            return Err("Git fields".into());
        };
        require(
            matches!(*mode, "100644" | "100755") && *kind == "blob",
            "Git regular",
        )?;
        let path = relative(name)?;
        let b = command(&["cat-file", "blob", blob])?;
        bytes_total = bytes_total.checked_add(b.len()).ok_or("source overflow")?;
        require(
            bytes_total <= 16 * 1024 * 1024 && b.len() <= 16 * 1024 * 1024,
            "source caps",
        )?;
        push(&mut material, (path, b, Some((*blob).to_string())), 1024)?;
    }
    for (name, sha) in [
        (
            "server/package.json",
            "f3f9375884302c7dcdd7fb510b7804f5dadcdced8ccabdc827638378353db19c",
        ),
        (
            "server/package-lock.json",
            "ad078a36d1f9b72ba7ffeb2fa945deca872ea81742c8fe1ed5c9323ddcf497e9",
        ),
        (
            "server/dav-parse.lock",
            "31f3a5c5c8da2cf0926db19f9ef742b49301b26c1d9c395a98980bb3050c4337",
        ),
    ] {
        require(
            material
                .iter()
                .any(|(p, b, _)| p == Path::new(name) && hash(b) == sha),
            "fixed prerequisite",
        )?;
    }
    let lock: Value = serde_json::from_slice(
        &material
            .iter()
            .find(|(p, _, _)| p == Path::new("server/package-lock.json"))
            .ok_or("lock")?
            .1,
    )
    .map_err(|_| "lock JSON")?;
    let rows = proof
        .get("files")
        .and_then(Value::as_array)
        .ok_or("proof rows")?;
    require(rows.len() == 1831, "exact dependencies")?;
    let mut expected = BTreeMap::new();
    let closure = proof
        .get("closure")
        .and_then(Value::as_array)
        .ok_or("closure")?
        .iter()
        .map(|v| v.as_str().map(str::to_string).ok_or("closure name".into()))
        .collect::<Result<BTreeSet<_>>>()?;
    let expected_roots = expected_package_roots(&closure)?;
    let mut manifests = BTreeSet::new();
    let mut deps_bytes = 0usize;
    for item in rows {
        require(
            started.elapsed() < Duration::from_secs(60),
            "materialize cooperative deadline",
        )?;
        let name = item
            .get("path")
            .and_then(Value::as_str)
            .ok_or("dependency path")?;
        let p = relative(name)?;
        require(p.starts_with("node_modules"), "dependency namespace")?;
        require(
            expected.insert(p.clone(), item).is_none(),
            "dependency duplicate",
        )?;
        let b = read(&Path::new(DEPS).join(&p), 16 * 1024 * 1024)?;
        require(
            item.get("bytes").and_then(Value::as_u64) == Some(b.len() as u64)
                && item.get("sha256").and_then(Value::as_str) == Some(hash(&b).as_str()),
            "dependency bytes",
        )?;
        if p.file_name() == Some(std::ffi::OsStr::new("package.json")) {
            let parent = p.parent().and_then(Path::to_str).ok_or("package parent")?;
            // Nested package.json files (e.g. an esm type marker) remain byte
            // attested files; only actual lock package roots define versions.
            if let Some(pinned) = lock.get("packages").and_then(|v| v.get(parent)) {
                let package: Value = serde_json::from_slice(&b).map_err(|_| "package JSON")?;
                validate_package_root(
                    parent,
                    &package,
                    pinned,
                    &closure,
                    &expected_roots,
                    &mut manifests,
                )?;
            }
        }
        deps_bytes = deps_bytes.checked_add(b.len()).ok_or("dep overflow")?;
        require(deps_bytes <= 64 * 1024 * 1024, "dep total cap")?;
        push(&mut material, (p, b, None), 5120)?;
    }
    require(deps_bytes == 19_185_089, "dependency total identity")?;
    validate_package_root_closure(&manifests, &expected_roots)?;
    let mut queue = vec![(PathBuf::from(DEPS).join("node_modules"), 0usize)];
    let mut visited = 0usize;
    let mut found = BTreeSet::new();
    while let Some((p, depth)) = queue.pop() {
        visited += 1;
        require(visited <= 8192 && depth <= 32, "traversal caps")?;
        let m = fs::symlink_metadata(&p).map_err(|_| "dependency stat")?;
        require(!m.file_type().is_symlink(), "dependency links")?;
        if m.is_dir() {
            require(
                (m.permissions().mode() & 0o777) == 0o700,
                "current dependency directory mode",
            )?;
            for entry in fs::read_dir(&p).map_err(|_| "dependency directory")? {
                push(
                    &mut queue,
                    (entry.map_err(|_| "dependency entry")?.path(), depth + 1),
                    4096,
                )?;
            }
        } else {
            let name = p.strip_prefix(DEPS).map_err(|_| "dependency root")?;
            require(
                expected.contains_key(name)
                    && m.is_file()
                    && m.nlink() == 1
                    && (m.permissions().mode() & 0o777) == 0o600,
                "dependency full mode inventory",
            )?;
            found.insert(name.to_path_buf());
        }
    }
    require(found.len() == 1831, "complete dependency traversal")?;
    push(
        &mut material,
        (PathBuf::from("server/wasm/dav-parse.wasm"), wasm, None),
        5120,
    )?;
    push(
        &mut material,
        (
            PathBuf::from("qualification-adapter.cjs"),
            adapter.as_bytes().to_vec(),
            None,
        ),
        5120,
    )?;
    directory(root)?;
    let canonical = fs::canonicalize(root).map_err(|_| "canonical output")?;
    require(
        canonical == Path::new("/private/tmp/m4-storage-factory-oracle-b375-stage-b"),
        "fixed canonical alias",
    )?;
    let mut inventory = Vec::new();
    for (name, b, blob) in material {
        require(
            started.elapsed() < Duration::from_secs(60),
            "materialize cooperative deadline",
        )?;
        let p = root.join(&name);
        let parent = p.parent().ok_or("parent")?;
        fs::create_dir_all(parent).map_err(|_| "owned directories")?;
        let mut dir = parent;
        loop {
            require(dir.starts_with(root), "owned parent")?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(|_| "directory mode")?;
            if dir == root {
                break;
            }
            dir = dir.parent().ok_or("parent")?;
        }
        put(&p, &b)?;
        require(read(&p, 16 * 1024 * 1024)? == b, "copied bytes")?;
        push(
            &mut inventory,
            json!({"path":name,"bytes":b.len(),"sha256":hash(&b),"mode":"100600","git_blob":blob}),
            5120,
        )?;
    }
    let input = cases()?;
    let manifest = json!({"core":CORE,"tree":TREE,"candidate":CANDIDATE,"canonical_root":canonical,"node_sha256":NODE_SHA,"wasm_sha256":WASM_SHA,"dependency_proof_sha256":PROOF_SHA,"files":inventory,"cases_sha256":hash(&json_bytes(&json!(input),8*1024*1024)?),"adapter_sha256":hash(adapter.as_bytes()),"oracle_executed":false,"runtime_metadata":"not captured yet","qualification":"logical caps, trusted stable ancestry; no registry/dylib/RSS/OOM/kernel certificate"});
    put(
        &root.join("manifest.json"),
        &json_bytes(&manifest, 2 * 1024 * 1024)?,
    )?;
    Ok(manifest)
}

/// Fresh observations, separate from the historical path/bytes/hash proof.
pub fn verify_materialized(manifest: &Value) -> Result<()> {
    let root = Path::new(ROOT);
    let rows = manifest
        .get("files")
        .and_then(Value::as_array)
        .ok_or("manifest files")?;
    require(rows.len() <= 5120, "inventory cap")?;
    require(
        manifest.get("core") == Some(&json!(CORE))
            && manifest.get("tree") == Some(&json!(TREE))
            && manifest.get("candidate") == Some(&json!(CANDIDATE)),
        "manifest immutable identity",
    )?;
    let mut expected = BTreeSet::new();
    for row in rows {
        let relative = relative(string(row, "path")?)?;
        require(expected.insert(relative.clone()), "manifest duplicate")?;
        let path = root.join(relative);
        let m = fs::symlink_metadata(&path).map_err(|_| "inventory stat")?;
        require(
            m.is_file() && m.nlink() == 1 && (m.permissions().mode() & 0o777) == 0o600,
            "fresh file mode/class/link count",
        )?;
        let bytes = read(&path, 16 * 1024 * 1024)?;
        require(
            row.get("bytes").and_then(Value::as_u64) == Some(bytes.len() as u64)
                && row.get("sha256").and_then(Value::as_str) == Some(hash(&bytes).as_str()),
            "fresh file bytes",
        )?;
    }
    let mut queue = vec![(root.to_path_buf(), 0usize)];
    let mut found = BTreeSet::new();
    let mut visited = 0usize;
    while let Some((path, depth)) = queue.pop() {
        visited += 1;
        require(visited <= 8192 && depth <= 32, "materialized traversal cap")?;
        let m = fs::symlink_metadata(&path).map_err(|_| "materialized stat")?;
        require(!m.file_type().is_symlink(), "materialized symlink")?;
        if m.is_dir() {
            require(
                m.permissions().mode() & 0o777 == 0o700,
                "materialized directory mode",
            )?;
            let name = path.strip_prefix(root).map_err(|_| "materialized root")?;
            if !name.as_os_str().is_empty() {
                require(
                    expected.iter().any(|p| p.starts_with(name)),
                    "extra materialized directory",
                )?;
            }
            for entry in fs::read_dir(&path).map_err(|_| "materialized directory")? {
                push(
                    &mut queue,
                    (entry.map_err(|_| "materialized entry")?.path(), depth + 1),
                    4096,
                )?;
            }
        } else {
            let name = path.strip_prefix(root).map_err(|_| "materialized root")?;
            require(
                m.is_file() && m.nlink() == 1 && m.permissions().mode() & 0o777 == 0o600,
                "materialized complete file class/mode",
            )?;
            require(
                expected.contains(name) || name == Path::new("manifest.json"),
                "extra materialized file",
            )?;
            if expected.contains(name) {
                found.insert(name.to_path_buf());
            }
        }
    }
    require(found == expected, "complete materialized inventory")?;
    require(
        hash(&read(Path::new(NODE), 128 * 1024 * 1024)?) == NODE_SHA,
        "fresh executable binding",
    )?;
    require(
        hash(&read(Path::new(NODE_CRYPTO_LIBRARY), 128 * 1024 * 1024)?) == NODE_CRYPTO_LIBRARY_SHA,
        "fixed native crypto error dialect bytes",
    )?;
    Ok(())
}

pub fn retain_capture(dir: &Path, stem: &str, o: &crate::process::Output) -> Result<()> {
    put(&dir.join(format!("{stem}.stdout")), &o.stdout)?;
    put(&dir.join(format!("{stem}.stderr")), &o.stderr)?;
    put(
        &dir.join(format!("{stem}.status.json")),
        &json_bytes(
            &json!({"code":o.status.and_then(|s|s.code()),"status_debug":o.status.map(|s|format!("{s:?}")),"success":o.status.map(|s|s.success()),"failure":o.failure.as_ref().map(|e|format!("{e:?}")),"diagnostic":o.diagnostic,"reap_error":o.reap_error}),
            4096,
        )?,
    )
}

pub fn approved_manifest() -> Result<Value> {
    require(
        APPROVED_MANIFEST_SHA.len() == 64,
        "root manifest approval held",
    )?;
    let bytes = read(&Path::new(ROOT).join("manifest.json"), 2 * 1024 * 1024)?;
    require(
        hash(&bytes) == APPROVED_MANIFEST_SHA,
        "root approved manifest bytes",
    )?;
    let manifest = serde_json::from_slice(&bytes).map_err(|_| "manifest JSON")?;
    verify_materialized(&manifest)?;
    Ok(manifest)
}

/// The comparison deliberately requires an owner and the complete synthetic plaintext.
/// It does not normalize an owner discrepancy or omit a credential from comparison.
pub fn compare_private_projection(expected: &Value, actual: &Value) -> Result<()> {
    for field in [
        "owner",
        "secret",
        "kind",
        "baseUrl",
        "bucket",
        "region",
        "username",
        "corpusRoot",
        "secretConfigured",
        "secretNeedsReauth",
        "effectiveRole",
    ] {
        let e = expected
            .get(field)
            .ok_or("missing expected projection field")?;
        let a = actual.get(field).ok_or("missing actual projection field")?;
        require(e == a, "private projection mismatch")?;
    }
    Ok(())
}

pub fn assert_missing_factory_shape(raw: &Value) -> Result<()> {
    let object = raw.as_object().ok_or("raw missing-row object")?;
    require(object.len() == 6, "raw missing-row exact shape")?;
    for field in ["secret", "secretConfigured", "secretNeedsReauth"] {
        require(
            !object.contains_key(field),
            "raw missing-row property must be absent",
        )?;
    }
    for field in [
        "kind",
        "baseUrl",
        "bucket",
        "region",
        "username",
        "corpusRoot",
    ] {
        require(object.contains_key(field), "raw missing-row metadata field")?;
    }
    Ok(())
}

/// Finite schema declarations owned by Rust, independent of adapter outcomes.
pub fn storage_schema_evidence_contract(case: &Value) -> Result<Value> {
    let declaration = match string(case, "shape")? {
        "view" => "selected user_id-only row; secret property absent",
        "missing" => "factory missing-table exception; no successful row evidence",
        "" | "loose" | "missing-region" | "no-pk" | "composite-pk" => {
            "declared secret column; retain selected full row or row absence"
        }
        _ => return Err("unknown schema evidence contract".into()),
    };
    Ok(json!({"sql":STORAGE_EVIDENCE_SQL,"declaration":declaration}))
}

/// The fixed S-view factory succeeds; its absent column is evidence, not an error.
pub fn assert_view_factory_evidence(case: &Value, node: &Value) -> Result<()> {
    require(
        string(case, "id")? == "S-view"
            && string(case, "shape")? == "view"
            && string(case, "owner")? == "a",
        "fixed view evidence case",
    )?;
    storage_schema_evidence_contract(case)?;
    require(
        node.get("id") == case.get("id")
            && node.get("owner") == case.get("owner")
            && node.get("exceptionPhase").is_none(),
        "view actual factory success binding",
    )?;
    require(
        node.get("storedEvidence")
            == Some(&json!({"presence":"property-missing","row":{"user_id":{"t":"text","v":"a"}}})),
        "view full-row missing secret property",
    )?;
    require(
        node.get("stored").is_none(),
        "view raw stored secret must be absent",
    )?;
    for field in ["storedPlain", "currentOpen", "foreignOpen"] {
        require(
            node.get(field).is_some_and(Value::is_null),
            "view absent-property opening evidence",
        )?;
    }
    require(
        node.get("storage")
            == Some(
                &json!({"bucket":"","region":"us-east-1","secret":"","secretConfigured":false,"secretNeedsReauth":false}),
            ),
        "view raw factory property contract",
    )?;
    let before = node
        .get("before")
        .and_then(Value::as_array)
        .ok_or("view before state")?;
    let after = node
        .get("after")
        .and_then(Value::as_array)
        .ok_or("view after state")?;
    require(
        before.len() == STATE_SQL.len() && before == after,
        "view exact measured SQL unchanged",
    )?;
    require(
        before.last() == Some(&json!({"columns":["user_id"],"rows":[[{"t":"text","v":"a"}]]})),
        "view exact SQL row/column contract",
    )
}

/// CORE openJs propagates native decipher.final() without a WASM wrapper.
/// The installed fixed libnode carries this exact CipherBase::Final error text.
/// This strict observation contract rejects all success, null and unknown faults.
pub fn assert_js_gcm_auth_failure(open: &Value) -> Result<()> {
    require(
        open == &json!({"error":{"name":"Error","code":null,"reason":null,"message":JS_GCM_AUTH_FAILURE}}),
        "foreign owner lacks exact native JS GCM authentication failure",
    )
}

pub const POLICY_DIFFERENCES: &[&str] = &[
    "readonly_no_lazy_reseal",
    "readonly_no_key_generation_or_chmod",
    "strict_row_types_and_schema",
    "private_logical_caps",
    "unknown_text_kind_local",
    "missingrow_public_flags_absent",
    "factory_only_headers_and_gate_composition",
    "private_no_auth_upkeep",
    "nontext_owner_rows_are_not_selected",
    "empty_owner_node_v1_unbound_candidate_refused",
];

pub const STATE_SQL: &[&str] = &[
    "SELECT * FROM users ORDER BY id",
    "SELECT * FROM settings ORDER BY key",
    "SELECT * FROM sessions ORDER BY id_hash",
    "SELECT * FROM device_grants ORDER BY id",
    "SELECT * FROM device_tokens ORDER BY token_hash",
    "SELECT * FROM audit_events ORDER BY id",
    "SELECT * FROM storage_connections ORDER BY user_id",
];
pub fn string<'a>(v: &'a Value, field: &str) -> Result<&'a str> {
    v.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("fixed {field}"))
}
pub fn decode_hex(s: &str) -> Result<Vec<u8>> {
    require(
        s.len() <= 262144 && s.len().is_multiple_of(2),
        "synthetic hex cap",
    )?;
    s.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| {
            std::str::from_utf8(p)
                .map_err(|_| "hex".to_string())
                .and_then(|p| u8::from_str_radix(p, 16).map_err(|_| "hex".into()))
        })
        .collect()
}
pub fn fixture(dir: &Path, case: &Value, with_row: bool) -> Result<()> {
    use server_store::rusqlite::{params, Connection};
    directory(dir)?;
    let db = Connection::open(dir.join("cowork.db")).map_err(|_| "synthetic database")?;
    db.execute_batch(SCHEMA).map_err(|_| "synthetic schema")?;
    for owner in BTreeSet::from(["a", "b", "foreign", string(case, "owner")?]) {
        db.execute("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES(?1,?1,?1,?1,'admin','synthetic',?1,1,1)",[owner]).map_err(|_|"synthetic user")?;
    }
    db.execute(
        "INSERT INTO settings(key,value) VALUES('instance_id','synthetic-instance')",
        [],
    )
    .map_err(|_| "synthetic setting")?;
    for owner in ["a", "b"] {
        let session = format!("synthetic-session-{owner}");
        let csrf = format!("synthetic-csrf-{owner}");
        db.execute(
            "INSERT INTO sessions VALUES(?1,?2,?3,?4,?4,?5,'synthetic','synthetic')",
            params![
                hash(session.as_bytes()),
                owner,
                hash(csrf.as_bytes()),
                NOW,
                NOW + 86400000
            ],
        )
        .map_err(|_| "synthetic session")?;
    }
    db.execute("INSERT INTO device_grants VALUES('synthetic-grant-a','a','synthetic',?1,?1,?2,'synthetic','synthetic')",params![NOW,NOW+86400000]).map_err(|_|"synthetic device grant")?;
    db.execute(
        "INSERT INTO device_tokens VALUES(?1,'synthetic-grant-a','access',?2,?3,NULL,NULL)",
        params![hash(b"nva_synthetic-device-a"), NOW, NOW + 86400000],
    )
    .map_err(|_| "synthetic device token")?;
    let id = string(case, "id")?;
    if id == "G-expired" {
        db.execute("UPDATE sessions SET expires_at=?1 WHERE user_id='a'", [NOW])
            .map_err(|_| "synthetic expiry")?;
    }
    if id == "G-disabled" {
        db.execute("UPDATE users SET disabled_at=1 WHERE id='a'", [])
            .map_err(|_| "synthetic disable")?;
    }
    if with_row {
        apply_row(&db, case)?;
    }
    db.close().map_err(|_| "synthetic close")?;
    for (field, name) in [
        ("current", "secrets.key"),
        ("previous", "secrets.key.previous"),
    ] {
        if let Some(s) = case.get(field).and_then(Value::as_str) {
            put(&dir.join(name), &decode_hex(s)?)?;
        }
    }
    if case.get("previous_directory").and_then(Value::as_bool) == Some(true) {
        directory(&dir.join("secrets.key.previous"))?;
    }
    put(
        &dir.join("first-run-setup-code"),
        b"synthetic-not-an-account-credential\n",
    )?;
    Ok(())
}
pub fn row_columns(case: &Value) -> Result<Vec<&'static str>> {
    if case.get("row").is_none_or(Value::is_null)
        || matches!(string(case, "shape")?, "missing" | "view")
    {
        return Ok(vec![]);
    }
    Ok([
        "user_id",
        "kind",
        "base_url",
        "bucket",
        "region",
        "username",
        "secret",
        "corpus_root",
        "updated_at",
    ]
    .into_iter()
    .filter(|n| {
        !(*n == "region" && case.get("shape").and_then(Value::as_str) == Some("missing-region"))
    })
    .collect())
}
pub fn insert_sql(case: &Value) -> Result<String> {
    let cols = row_columns(case)?;
    if cols.is_empty() {
        return Ok(String::new());
    }
    Ok(format!(
        "INSERT INTO storage_connections({}) VALUES({})",
        cols.join(","),
        vec!["?"; cols.len()].join(",")
    ))
}
pub fn cell_value(cell: &Value) -> Result<server_store::rusqlite::types::Value> {
    use server_store::rusqlite::types::Value as Sql;
    let v = cell.get("v").ok_or("cell value")?;
    Ok(match string(cell, "t")? {
        "text" => Sql::Text(v.as_str().ok_or("text cell")?.into()),
        "integer" => Sql::Integer(v.as_i64().ok_or("integer cell")?),
        "real" => Sql::Real(v.as_f64().ok_or("real cell")?),
        "blob" => Sql::Blob(decode_hex(v.as_str().ok_or("blob cell")?)?),
        "null" => Sql::Null,
        _ => return Err("fixed cell type".into()),
    })
}
pub fn apply_row(db: &server_store::rusqlite::Connection, case: &Value) -> Result<()> {
    db.execute_batch(&shape_sql(string(case, "shape")?)?)
        .map_err(|_| "synthetic shape")?;
    let sql = insert_sql(case)?;
    if !sql.is_empty() {
        let values = row_columns(case)?
            .into_iter()
            .map(|c| {
                case.get("row")
                    .and_then(|r| r.get(c))
                    .ok_or("row cell".into())
                    .and_then(cell_value)
            })
            .collect::<Result<Vec<_>>>()?;
        db.execute(&sql, server_store::rusqlite::params_from_iter(values))
            .map_err(|_| "synthetic row")?;
    }
    if let Some(foreign) = foreign_case(case)? {
        let values = row_columns(&foreign)?
            .into_iter()
            .map(|c| {
                foreign
                    .get("row")
                    .and_then(|r| r.get(c))
                    .ok_or("foreign cell".into())
                    .and_then(cell_value)
            })
            .collect::<Result<Vec<_>>>()?;
        db.execute(
            &insert_sql(&foreign)?,
            server_store::rusqlite::params_from_iter(values),
        )
        .map_err(|_| "foreign row")?;
    }
    Ok(())
}

pub fn foreign_case(case: &Value) -> Result<Option<Value>> {
    if string(case, "lane")? == "gate" || matches!(string(case, "shape")?, "missing" | "view") {
        return Ok(None);
    }
    let mut foreign = case.clone();
    *foreign.get_mut("owner").ok_or("foreign owner slot")? = json!("foreign");
    let mut row = row();
    *row.get_mut("user_id").ok_or("foreign row owner slot")? = json!({"t":"text","v":"foreign"});
    *row.get_mut("secret").ok_or("foreign secret slot")? = json!({"t":"text","v":"foreign-secret"});
    *foreign.get_mut("row").ok_or("foreign row slot")? = row;
    Ok(Some(foreign))
}
pub fn state(dir: &Path) -> Result<Value> {
    use server_store::rusqlite::{types::ValueRef, Connection, OpenFlags};
    let db = Connection::open_with_flags(dir.join("cowork.db"), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| "state read")?;
    let mut tables = Vec::new();
    for sql in STATE_SQL {
        let result = (|| {
            let mut stmt = db.prepare(sql).map_err(|_| "state query")?;
            let columns = stmt
                .column_names()
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>();
            let rows = stmt
                .query_map([], |row| {
                    let mut values = Vec::new();
                    for i in 0..columns.len() {
                        let v = match row.get_ref(i)? {
                            ValueRef::Null => json!({"t":"null","v":null}),
                            ValueRef::Integer(v) => json!({"t":"integer","v":v}),
                            ValueRef::Real(v) => json!({"t":"real","v":v}),
                            ValueRef::Text(v) => json!({"t":"text","v":String::from_utf8_lossy(v)}),
                            ValueRef::Blob(v) => json!({"t":"blob","v":hex(v)}),
                        };
                        values.push(v);
                    }
                    Ok(values)
                })
                .map_err(|_| "state rows")?;
            let mut values = Vec::new();
            for row in rows {
                push(&mut values, row.map_err(|_| "state row")?, 128)?;
            }
            Ok::<_, String>(json!({"columns":columns,"rows":values}))
        })();
        tables.push(result.unwrap_or_else(|_| json!({"unavailable":true})));
    }
    Ok(json!(tables))
}
pub fn candidate(dir: &Path, case: &Value) -> Result<Value> {
    use server_auth::{
        secrets::SecretKeys, AuthConfig, Authenticator, Credential, Creds, Identity, PublicUser,
        Role,
    };
    use server_storage_reads::owner_secret::project_connection;
    let before = state(dir)?;
    let result: Result<Value> = (|| {
        let store = server_store::Store::open(dir).map_err(|_| "store refused".to_string())?;
        let keys = SecretKeys::load(dir, case.get("previous_env").and_then(Value::as_str))
            .map_err(|_| "key refused".to_string())?;
        let identity = Identity {
            user: PublicUser {
                id: string(case, "owner")?.into(),
                username: "synthetic".into(),
                display_name: "synthetic".into(),
                role: Role::Member,
                disabled: false,
                diary_enabled: false,
                onboarded: true,
            },
            account_role: Role::Admin,
            credential: Credential::Legacy,
        };
        let gate_lane = string(case, "lane")? == "gate";
        let headers = case
            .get("headers")
            .and_then(Value::as_object)
            .ok_or("headers")?;
        let creds = Creds::from_headers(
            string(case, "method")?,
            headers
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.as_str(), v.as_bytes()))),
        );
        let auth = Authenticator::new(AuthConfig {
            public_origin: "https://owner.invalid".into(),
            additional_origins: vec![],
            legacy_token: String::new(),
            legacy_compat: false,
            trust_proxy: true,
            native_client_auth_env: case.get("device_enabled").and_then(Value::as_bool),
            origin_from_settings: false,
        });
        let mut components = Value::Null;
        let mut gate = Value::Null;
        let selected = if gate_lane {
            let authn = auth
                .authenticate(&store, &creds, NOW)
                .map_err(|_| "auth read")?;
            let origin = store
                .read(|r| auth.origin_valid(r, &creds))
                .map_err(|_| "origin read")?;
            components = json!({"owner":authn.as_ref().map(|i|i.user.id.as_str()),"role":authn.as_ref().map(|i|i.user.role.as_str()),"device":authn.as_ref().map(Identity::is_device),"originValid":origin,"csrfValid":authn.as_ref().map(|i|auth.csrf_valid(&creds,i)),"browserOnly":authn.as_ref().map(|i|server_auth::identity::browser_only(i,string(case,"path").unwrap_or(""),string(case,"method").unwrap_or("")))});
            match auth
                .gate(&store, &creds, string(case, "path")?, NOW)
                .map_err(|_| "gate read")?
            {
                Ok(id) => {
                    gate = json!("accepted");
                    Some(id)
                }
                Err(r) => {
                    gate = json!(format!("{r:?}"));
                    None
                }
            }
        } else {
            Some(identity)
        };
        let Some(identity) = selected else {
            return Ok(
                json!({"components":components,"gate":gate,"projection_count":0,"projection":null}),
            );
        };
        let p = project_connection(&store, &identity, &keys)
            .map_err(|e| format!("projection:{e:?}"))?;
        let c = p.connection();
        Ok(
            json!({"components":components,"gate":gate,"projection_count":1,"projection":{"owner":identity.user.id,"effectiveRole":p.effective_role().as_str(),"kind":format!("{:?}",c.kind),"baseUrl":c.base_url,"bucket":c.bucket,"region":c.region,"username":c.username,"corpusRoot":c.corpus_root,"secret":String::from_utf8_lossy(c.secret.as_slice()),"secretConfigured":p.secret_configured(),"secretNeedsReauth":p.secret_needs_reauth()}}),
        )
    })();
    let after = state(dir)?;
    require(before == after, "candidate SQL changed")?;
    Ok(
        json!({"result":result.unwrap_or_else(|e|json!({"refusal":e})),"sql_before":before,"sql_after":after,"readonly":true,"identity_truth_limit":"nongate fixture identities are publicly constructible; only gate lane uses actual gate"}),
    )
}
