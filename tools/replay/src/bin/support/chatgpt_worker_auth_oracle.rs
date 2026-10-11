//! Private synthetic auth/device oracle contract; never imported by production.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path},
};
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub const IDS: [&str; 5] = ["live", "idle", "device", "mixed", "unauthorized"];
pub const CORE: &str = "010735c7d86d30269ab3f4a351b2135faea1c8e7";
pub const TREE: &str = "345adfa12da083eb5888ec232dc2d86b9b0383a5";
pub const NOW: i64 = 1_800_000_000_000;
pub const SCHEMA: &str =
    include_str!("../../../../../crates/server-store/tests/fixtures/node-schema.sql");
pub const SETUP: &str =
    include_str!("../../../../../bins/noevia-server/tests/support/chatgpt_worker_fixture.sql");
pub const ADAPTER: &str = r#"'use strict';
const path=require('node:path');
const fs=require('node:fs');
const input=JSON.parse(fs.readFileSync(0,'utf8'));
const {checkout,dataDir,cookie,bearer}=input;
const Database=require(path.join(checkout,'node_modules/better-sqlite3'));
const seed=new Database(path.join(dataDir,'cowork.db')); try { seed.exec(input.schema); seed.exec(input.setup); } finally { seed.close(); }
const from=m=>require(path.join(checkout,'server',m));
const now=1800000000000; Date.now=()=>now;
const {createAuth,createRateLimiter}=from('auth.cjs');
const lib=from('device-auth.cjs');
const a=createAuth({dataDir,publicOrigin:'',rpId:'',trustProxy:true});
let payload;
try {
const d=lib.createDeviceAuth({db:a.db,audit:()=>{},publicUser:a.publicUser,rate:createRateLimiter(),clientAddress:()=> '127.0.0.1',origin:()=>a.origin,now:()=>now,addressesTrusted:true});
const r=lib.createRequestAuth({enabled:()=>true,deviceAuth:d,authService:a});
const req={method:'GET',headers:{cookie,authorization:bearer},socket:{remoteAddress:'127.0.0.1'}};
const id=r.authenticate(req);
payload={authenticated:!!id,browserOnly:r.browserOnly(id,'/api/providers/chatgpt','GET'),sessions:a.db.prepare('SELECT id_hash,last_seen_at FROM sessions ORDER BY id_hash').all(),grants:a.db.prepare('SELECT id,last_used_at FROM device_grants ORDER BY id').all()};
} finally { a.db.close(); }
process.stdout.write(JSON.stringify({node:process.version,abi:process.versions.modules,platform:process.platform,payload,modules:Object.keys(require.cache).sort()}));
"#;
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn setup_sql() -> String {
    SETUP
        .replace("__LIVE__", &hash(b"live"))
        .replace("__IDLE__", &hash(b"idle"))
        .replace("__DEVICE__", &hash(b"nva_device"))
        .replace("__LIVE_SEEN__", &(NOW - 1000).to_string())
        .replace("__IDLE_SEEN__", &(NOW - 604800000).to_string())
        .replace("__GRANT_SEEN__", &(NOW - 60000).to_string())
        .replace("__EXPIRES__", &(NOW + 99999).to_string())
}
fn require(ok: bool, why: &'static str) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(why.into())
    }
}
pub fn field<'a>(v: &'a Value, name: &str) -> &'a Value {
    v.get(name).unwrap_or(&Value::Null)
}
fn keys(v: &Value, names: &[&str]) -> Result<()> {
    let o = v.as_object().ok_or("object required")?;
    require(
        o.len() == names.len() && names.iter().all(|n| o.contains_key(*n)),
        "exact object keys",
    )
}
pub fn validate(rows: &[Value]) -> Result<()> {
    require(rows.len() == 5, "five exact worker rows")?;
    for (row, id) in rows.iter().zip(IDS) {
        keys(
            row,
            &["id", "authenticated", "browserOnly", "sessions", "grants"],
        )?;
        require(
            field(row, "id") == id
                && field(row, "authenticated").is_boolean()
                && field(row, "browserOnly").is_boolean(),
            "worker id/booleans",
        )?;
        for (name, cap, key, time) in [
            ("sessions", 2, "id_hash", "last_seen_at"),
            ("grants", 1, "id", "last_used_at"),
        ] {
            let a = field(row, name).as_array().ok_or("SQL rows")?;
            require(a.len() <= cap, "SQL row cap")?;
            let mut previous = None;
            for v in a {
                keys(v, &[key, time])?;
                let text = field(v, key).as_str().ok_or("SQL text")?;
                require(
                    if key == "id_hash" {
                        text.len() == 64 && text.bytes().all(|c| c.is_ascii_hexdigit())
                    } else {
                        text == "g"
                    },
                    "fixed synthetic SQL key",
                )?;
                require(
                    field(v, time).as_i64().is_some() && previous.is_none_or(|p| p < text),
                    "SQL ordered timestamp",
                )?;
                previous = Some(text);
            }
        }
    }
    Ok(())
}
pub fn differences(expected: &[Value], actual: &[Value]) -> Result<Vec<String>> {
    validate(expected)?;
    validate(actual)?;
    let mut changes = Vec::new();
    for ((a, b), id) in expected.iter().zip(actual).zip(IDS) {
        for name in ["authenticated", "browserOnly"] {
            if field(a, name) != field(b, name) {
                changes.push(format!("{id}/{name}"));
            }
        }
        for (name, key, time) in [
            ("sessions", "id_hash", "last_seen_at"),
            ("grants", "id", "last_used_at"),
        ] {
            let aa = field(a, name).as_array().ok_or("expected SQL")?;
            let bb = field(b, name).as_array().ok_or("actual SQL")?;
            if aa.len() != bb.len() {
                changes.push(format!("{id}/{name}/length"));
            }
            for (i, (a, b)) in aa.iter().zip(bb).enumerate() {
                for k in [key, time] {
                    if field(a, k) != field(b, k) {
                        changes.push(format!("{id}/{name}/{i}/{k}"));
                    }
                }
            }
        }
    }
    Ok(changes)
}
fn read(path: &Path, cap: usize) -> Result<Vec<u8>> {
    let f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC)
                .bits() as i32,
        )
        .open(path)?;
    let m = f.metadata()?;
    require(
        m.is_file() && m.nlink() == 1 && m.mode() & 0o7777 == 0o600 && m.len() <= cap as u64,
        "private reference descriptor",
    )?;
    let mut bytes = Vec::new();
    f.take((cap + 1) as u64).read_to_end(&mut bytes)?;
    require(bytes.len() <= cap, "reference cap")?;
    Ok(bytes)
}
fn bound(root: &Path, name: &str, sha: &Value, cap: usize) -> Result<Vec<u8>> {
    let b = read(&root.join(name), cap)?;
    require(
        sha.as_str() == Some(hash(&b).as_str()),
        "reference file hash",
    )?;
    Ok(b)
}
pub fn reference_at(value: &str) -> Result<Vec<Value>> {
    require(
        value.len() <= 4096
            && value
                .split('/')
                .skip(1)
                .all(|p| !p.is_empty() && p != "." && p != ".."),
        "binding path cap/grammar",
    )?;
    let path = Path::new(value);
    require(
        path.is_absolute()
            && path
                .components()
                .all(|p| matches!(p, Component::Normal(_) | Component::RootDir))
            && fs::canonicalize(path)? == path,
        "canonical binding",
    )?;
    let root = path.parent().ok_or("binding parent")?;
    require(
        path.file_name().is_some_and(|n| n == "worker-binding.json")
            && fs::symlink_metadata(root)?.mode() & 0o7777 == 0o700,
        "owned binding root",
    )?;
    let binding: Value = serde_json::from_slice(&read(path, 16384)?)?;
    require(
        field(&binding, "version") == 1
            && field(&binding, "root").as_str() == root.to_str()
            && field(&binding, "core_commit") == CORE
            && field(&binding, "core_tree") == TREE
            && field(&binding, "now") == NOW
            && field(&binding, "ids") == &json!(IDS),
        "fixed worker binding",
    )?;
    require(
        field(&binding, "schema_sha256").as_str() == Some(hash(SCHEMA.as_bytes()).as_str())
            && field(&binding, "setup_sha256").as_str() == Some(hash(SETUP.as_bytes()).as_str())
            && field(&binding, "adapter_sha256").as_str()
                == Some(hash(ADAPTER.as_bytes()).as_str())
            && field(&binding, "test_sha256").as_str()
                == Some(
                    hash(include_bytes!(
                        "../../../../../bins/noevia-server/tests/chatgpt_worker.rs"
                    ))
                    .as_str(),
                ),
        "compiled worker source binding",
    )?;
    require(
        field(field(&binding, "runtime"), "version") == "v22.23.3"
            && field(field(&binding, "runtime"), "abi") == "127"
            && field(field(&binding, "runtime"), "platform") == "linux",
        "Linux runtime binding",
    )?;
    let cert: Value = serde_json::from_slice(&bound(
        root,
        "worker-certificate.json",
        field(&binding, "certificate_sha256"),
        2 * 1024 * 1024,
    )?)?;
    let manifest = bound(
        root,
        "oracle/qualification-manifest.json",
        field(&binding, "manifest_sha256"),
        2 * 1024 * 1024,
    )?;
    let manifest: Value = serde_json::from_slice(&manifest)?;
    require(
        field(&manifest, "core_commit") == CORE
            && field(&manifest, "core_tree") == TREE
            && field(&manifest, "runtime") == field(&binding, "runtime")
            && field(&manifest, "node_sha256") == field(&binding, "node_sha256"),
        "manifest authority",
    )?;
    let manifest_rows = field(&manifest, "files")
        .as_array()
        .ok_or("manifest files")?;
    require(
        !manifest_rows.is_empty() && manifest_rows.len() <= 5120,
        "manifest count",
    )?;
    let oracle = root.join("oracle");
    require(
        fs::canonicalize(&oracle)? == oracle,
        "canonical oracle root",
    )?;
    let verify_loaded = |loaded: &Value, modules: &Value| -> Result<()> {
        let loaded = loaded.as_array().ok_or("loaded attestation")?;
        let modules = modules.as_array().ok_or("module trace")?;
        require(
            !loaded.is_empty() && loaded.len() <= 512 && loaded.len() == modules.len(),
            "loaded trace count",
        )?;
        let mut seen = std::collections::BTreeSet::new();
        for (row, module) in loaded.iter().zip(modules) {
            let name = field(row, "path").as_str().ok_or("loaded relative path")?;
            let path = Path::new(name);
            require(
                name.len() <= 4096
                    && path.components().all(|c| matches!(c, Component::Normal(_)))
                    && seen.insert(name),
                "loaded namespace",
            )?;
            require(
                manifest_rows.iter().filter(|m| *m == row).count() == 1,
                "loaded manifest identity",
            )?;
            let file = oracle.join(path);
            require(
                fs::canonicalize(&file)? == file && module.as_str() == file.to_str(),
                "loaded exact origin",
            )?;
            let bytes = bound(&oracle, name, field(row, "sha256"), 16 * 1024 * 1024)?;
            require(
                field(row, "bytes").as_u64() == Some(bytes.len() as u64),
                "loaded byte count",
            )?;
        }
        Ok(())
    };
    let native: Value = serde_json::from_slice(&bound(
        root,
        "native-preflight.stdout",
        field(&cert, "native_stdout_sha256"),
        1024 * 1024,
    )?)?;
    require(
        bound(
            root,
            "native-preflight.stderr",
            field(&cert, "native_stderr_sha256"),
            64 * 1024,
        )?
        .is_empty(),
        "native stderr",
    )?;
    let native_status: Value = serde_json::from_slice(&bound(
        root,
        "native-preflight.status.json",
        field(&cert, "native_status_sha256"),
        8192,
    )?)?;
    require(
        field(&native_status, "child_code") == 0
            && field(&native_status, "setup_error").is_null()
            && field(&native_status, "outer_completed") == true,
        "native actual child status",
    )?;
    require(
        field(&native, "version") == "v22.23.3"
            && field(&native, "abi") == "127"
            && field(&native, "platform") == "linux",
        "native actual dialect",
    )?;
    verify_loaded(field(&cert, "native_loaded"), field(&native, "modules"))?;
    let bytes = bound(
        root,
        "worker-reference.json",
        field(&binding, "reference_sha256"),
        1024 * 1024,
    )?;
    let rows: Vec<Value> = serde_json::from_slice(&bytes)?;
    validate(&rows)?;
    let certificate_rows: Vec<Value> = serde_json::from_value(field(&cert, "rows").clone())?;
    require(
        differences(&certificate_rows, &rows)?.is_empty()
            && field(&cert, "core_commit") == CORE
            && field(&cert, "adapter_sha256") == field(&binding, "adapter_sha256"),
        "certificate rows/source",
    )?;
    let captures = field(&cert, "captures")
        .as_array()
        .ok_or("five capture certificates")?;
    require(captures.len() == 5, "capture count")?;
    for ((capture, row), id) in captures.iter().zip(&rows).zip(IDS) {
        require(field(capture, "id") == id, "capture id")?;
        let raw = bound(
            root,
            &format!("capture-{id}.stdout"),
            field(capture, "stdout_sha256"),
            1024 * 1024,
        )?;
        let err = bound(
            root,
            &format!("capture-{id}.stderr"),
            field(capture, "stderr_sha256"),
            64 * 1024,
        )?;
        require(err.is_empty(), "empty child stderr")?;
        let status: Value = serde_json::from_slice(&bound(
            root,
            &format!("capture-{id}.status.json"),
            field(capture, "status_sha256"),
            8192,
        )?)?;
        require(
            field(&status, "child_code") == 0
                && field(&status, "setup_error").is_null()
                && field(&status, "outer_completed") == true,
            "actual child success",
        )?;
        let raw: Value = serde_json::from_slice(&raw)?;
        require(
            field(&raw, "node") == "v22.23.3"
                && field(&raw, "abi") == "127"
                && field(&raw, "platform") == "linux",
            "capture actual dialect",
        )?;
        verify_loaded(field(capture, "loaded"), field(&raw, "modules"))?;
        let input: Value = serde_json::from_slice(&bound(
            root,
            &format!("input-{id}.json"),
            field(capture, "input_sha256"),
            64 * 1024,
        )?)?;
        let pair = match id {
            "live" => ("cowork_session=live", ""),
            "idle" => ("cowork_session=idle", ""),
            "device" => ("", "Bearer nva_device"),
            "mixed" => ("cowork_session=live", "Bearer nva_device"),
            "unauthorized" => ("", ""),
            _ => return Err("case id".into()),
        };
        require(
            field(&input, "cookie") == pair.0
                && field(&input, "bearer") == pair.1
                && field(&input, "checkout").as_str() == oracle.to_str()
                && field(&input, "schema") == SCHEMA
                && field(&input, "setup").as_str() == Some(setup_sql().as_str())
                && field(&input, "dataDir").as_str()
                    == oracle.join("worker-fixtures").join(id).to_str(),
            "exact synthetic capture input",
        )?;
        let mut payload = field(&raw, "payload").clone();
        payload
            .as_object_mut()
            .ok_or("capture payload")?
            .insert("id".into(), json!(id));
        require(&payload == row, "raw semantic equality")?;
    }
    Ok(rows)
}
