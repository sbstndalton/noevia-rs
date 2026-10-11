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
// Conservative tracked package closure for the two compiled targets. Adding a module,
// dependency or include changes an already bound source/manifest; refresh this map
// when that closure changes. Registry implementations/compiler environment are outside it.
macro_rules! compiled_inputs {
    ($($name:literal),+ $(,)?) => { &[$(($name, include_bytes!(concat!("../../../../../", $name)) as &[u8])),+] };
}
fn compiled_inputs() -> &'static [(&'static str, &'static [u8])] {
    compiled_inputs![
        "Cargo.lock",
        "Cargo.toml",
        "bins/noevia-server/Cargo.toml",
        "bins/noevia-server/build.rs",
        "bins/noevia-server/src/account_routes.rs",
        "bins/noevia-server/src/bin/check-resolver-mutations.rs",
        "bins/noevia-server/src/chatgpt_status_routes.rs",
        "bins/noevia-server/src/chatgpt_worker.rs",
        "bins/noevia-server/src/code_net.rs",
        "bins/noevia-server/src/code_net_resolver.rs",
        "bins/noevia-server/src/config.rs",
        "bins/noevia-server/src/health.rs",
        "bins/noevia-server/src/identity.rs",
        "bins/noevia-server/src/legacy_proxy.rs",
        "bins/noevia-server/src/lib.rs",
        "bins/noevia-server/src/main.rs",
        "bins/noevia-server/src/project_routes.rs",
        "bins/noevia-server/src/reasoning_settings_routes.rs",
        "bins/noevia-server/src/reply.rs",
        "bins/noevia-server/src/routes.rs",
        "bins/noevia-server/src/routing_mode_routes.rs",
        "bins/noevia-server/src/sampling_settings_routes.rs",
        "bins/noevia-server/src/serve.rs",
        "bins/noevia-server/src/static_files.rs",
        "bins/noevia-server/src/support/chatgpt_status_differential.rs",
        "bins/noevia-server/src/upkeep.rs",
        "bins/noevia-server/src/usage_routes.rs",
        "bins/noevia-server/tests/account_routes.rs",
        "bins/noevia-server/tests/chatgpt_status_http.rs",
        "bins/noevia-server/tests/chatgpt_worker.rs",
        "bins/noevia-server/tests/code_net_resolver.rs",
        "bins/noevia-server/tests/features_cli.rs",
        "bins/noevia-server/tests/fixtures/client-routes.json",
        "bins/noevia-server/tests/front.rs",
        "bins/noevia-server/tests/identity.rs",
        "bins/noevia-server/tests/reasoning_settings.rs",
        "bins/noevia-server/tests/routing_mode.rs",
        "bins/noevia-server/tests/sampling_settings.rs",
        "bins/noevia-server/tests/support/chatgpt_status_http_controls.rs",
        "bins/noevia-server/tests/support/chatgpt_worker_fixture.sql",
        "bins/noevia-server/tests/support/resolver_controls.rs",
        "bins/noevia-server/tests/support/resolver_fixture.rs",
        "bins/noevia-server/tests/upkeep.rs",
        "bins/noevia-server/tests/usage.rs",
        "contracts/http/routes.toml",
        "crates/chatgpt-status/Cargo.lock",
        "crates/chatgpt-status/Cargo.toml",
        "crates/chatgpt-status/README.md",
        "crates/chatgpt-status/examples/mutants.rs",
        "crates/chatgpt-status/examples/node_oracle.rs",
        "crates/chatgpt-status/examples/sqlite_mutants.rs",
        "crates/chatgpt-status/examples/sqlite_oracle.rs",
        "crates/chatgpt-status/examples/support/sqlite_assertions.rs",
        "crates/chatgpt-status/src/json.rs",
        "crates/chatgpt-status/src/keys.rs",
        "crates/chatgpt-status/src/lib.rs",
        "crates/chatgpt-status/src/sqlite.rs",
        "crates/chatgpt-status/tests/keys.rs",
        "crates/chatgpt-status/tests/node-sqlite.v1.json",
        "crates/chatgpt-status/tests/node-status.v1.json",
        "crates/chatgpt-status/tests/oracle.cjs",
        "crates/chatgpt-status/tests/policy.rs",
        "crates/chatgpt-status/tests/sqlite-oracle.cjs",
        "crates/chatgpt-status/tests/sqlite.rs",
        "crates/chatgpt-status/tests/sqlite_classifier.rs",
        "crates/code-net-guard/Cargo.toml",
        "crates/code-net-guard/src/lib.rs",
        "crates/code-net-guard/tests/fixtures.rs",
        "crates/code-net-guard/tests/fixtures/code-net-guard.v1.json",
        "crates/code-net-guard/tests/properties.rs",
        "crates/js-json/Cargo.toml",
        "crates/js-json/src/lib.rs",
        "crates/js-json/tests/fixtures/stringify-pretty.v1.json",
        "crates/mcp-frame/Cargo.toml",
        "crates/mcp-frame/src/json.rs",
        "crates/mcp-frame/src/lib.rs",
        "crates/mcp-frame/src/schema.rs",
        "crates/mcp-frame/tests/differential.rs",
        "crates/mcp-frame/tests/fixtures/mcp-frame.v1.json",
        "crates/mcp-frame/tests/properties.rs",
        "crates/passkey/Cargo.toml",
        "crates/passkey/src/b64.rs",
        "crates/passkey/src/cbor.rs",
        "crates/passkey/src/cose.rs",
        "crates/passkey/src/lib.rs",
        "crates/passkey/tests/compat.rs",
        "crates/passkey/tests/fixtures/simplewebauthn-v14.json",
        "crates/passkey/tests/selfcheck.rs",
        "crates/policy-leaves/Cargo.toml",
        "crates/policy-leaves/src/lib.rs",
        "crates/policy-leaves/tests/fixtures.rs",
        "crates/policy-leaves/tests/fixtures/policy-leaves.v1.json",
        "crates/policy-leaves/tests/properties.rs",
        "crates/prompt-framing/Cargo.toml",
        "crates/prompt-framing/src/framing.rs",
        "crates/prompt-framing/src/js.rs",
        "crates/prompt-framing/src/json.rs",
        "crates/prompt-framing/src/lib.rs",
        "crates/prompt-framing/src/packet.rs",
        "crates/prompt-framing/src/provenance.rs",
        "crates/prompt-framing/tests/fixtures.rs",
        "crates/prompt-framing/tests/fixtures/prompt-framing.v1.json",
        "crates/prompt-framing/tests/properties.rs",
        "crates/sandbox-bridge/Cargo.toml",
        "crates/sandbox-bridge/src/contain.rs",
        "crates/sandbox-bridge/src/frame.rs",
        "crates/sandbox-bridge/src/js.rs",
        "crates/sandbox-bridge/src/json.rs",
        "crates/sandbox-bridge/src/lib.rs",
        "crates/sandbox-bridge/src/start.rs",
        "crates/sandbox-bridge/src/tool_call.rs",
        "crates/sandbox-bridge/tests/differential.rs",
        "crates/sandbox-bridge/tests/fixtures/sandbox-bridge.v1.json",
        "crates/sandbox-bridge/tests/properties.rs",
        "crates/secret-envelope/Cargo.toml",
        "crates/secret-envelope/src/lib.rs",
        "crates/secret-envelope/tests/differential.rs",
        "crates/secret-envelope/tests/fixtures/secret-envelope.v1.json",
        "crates/secret-envelope/tests/properties.rs",
        "crates/server-account/Cargo.toml",
        "crates/server-account/src/account.rs",
        "crates/server-account/src/argon.rs",
        "crates/server-account/src/device.rs",
        "crates/server-account/src/files.rs",
        "crates/server-account/src/lib.rs",
        "crates/server-account/src/open.rs",
        "crates/server-account/src/passkeys.rs",
        "crates/server-account/src/rate.rs",
        "crates/server-account/src/unicode.rs",
        "crates/server-account/src/users.rs",
        "crates/server-account/src/util.rs",
        "crates/server-account/tests/flows.rs",
        "crates/server-auth/Cargo.toml",
        "crates/server-auth/src/app_passwords.rs",
        "crates/server-auth/src/config.rs",
        "crates/server-auth/src/identity.rs",
        "crates/server-auth/src/js.rs",
        "crates/server-auth/src/lib.rs",
        "crates/server-auth/src/password.rs",
        "crates/server-auth/src/request.rs",
        "crates/server-auth/src/secrets.rs",
        "crates/server-auth/src/upkeep.rs",
        "crates/server-auth/tests/bounded_upkeep.rs",
        "crates/server-auth/tests/common/mod.rs",
        "crates/server-auth/tests/differential.rs",
        "crates/server-auth/tests/fixtures/node-auth.v1.json",
        "crates/server-auth/tests/properties.rs",
        "crates/server-projects/Cargo.toml",
        "crates/server-projects/src/assets.rs",
        "crates/server-projects/src/js.rs",
        "crates/server-projects/src/lib.rs",
        "crates/server-projects/src/lock.rs",
        "crates/server-projects/src/store.rs",
        "crates/server-projects/tests/node_lock.rs",
        "crates/server-reasoning-settings/Cargo.toml",
        "crates/server-reasoning-settings/src/input.rs",
        "crates/server-reasoning-settings/src/lib.rs",
        "crates/server-reasoning-settings/tests/differential.rs",
        "crates/server-reasoning-settings/tests/fixtures/node-reasoning.v1.json",
        "crates/server-reasoning-settings/tests/oracle.cjs",
        "crates/server-reasoning-settings/tests/policy.rs",
        "crates/server-routing-mode/Cargo.toml",
        "crates/server-routing-mode/examples/mutants.rs",
        "crates/server-routing-mode/examples/node_oracle.rs",
        "crates/server-routing-mode/examples/process_controls.rs",
        "crates/server-routing-mode/src/input.rs",
        "crates/server-routing-mode/src/lib.rs",
        "crates/server-routing-mode/src/storage.rs",
        "crates/server-routing-mode/tests/differential.rs",
        "crates/server-routing-mode/tests/fixtures/node-routing.v1.json",
        "crates/server-routing-mode/tests/policy.rs",
        "crates/server-routing-mode/tests/storage.rs",
        "crates/server-routing-mode/tests/support/mod.rs",
        "crates/server-sampling-settings/Cargo.toml",
        "crates/server-sampling-settings/README.md",
        "crates/server-sampling-settings/src/input.rs",
        "crates/server-sampling-settings/src/lib.rs",
        "crates/server-sampling-settings/tests/differential.rs",
        "crates/server-sampling-settings/tests/fixtures/node-sampling.v1.json",
        "crates/server-sampling-settings/tests/input.rs",
        "crates/server-sampling-settings/tests/store.rs",
        "crates/server-store/Cargo.toml",
        "crates/server-store/src/bounded.rs",
        "crates/server-store/src/json.rs",
        "crates/server-store/src/lib.rs",
        "crates/server-store/src/tests.rs",
        "crates/server-store/tests/bounded.rs",
        "crates/server-store/tests/fixtures/node-schema.sql",
        "crates/server-store/tests/node_guard.rs",
        "crates/server-store/tests/support/bounded_input.rs",
        "crates/server-usage/Cargo.toml",
        "crates/server-usage/README.md",
        "crates/server-usage/examples/timezones.rs",
        "crates/server-usage/src/input.rs",
        "crates/server-usage/src/lib.rs",
        "crates/server-usage/src/read.rs",
        "crates/server-usage/src/summary.rs",
        "crates/server-usage/src/timezone-names.json",
        "crates/server-usage/tests/differential.rs",
        "crates/server-usage/tests/fixtures/model-sort.json",
        "crates/server-usage/tests/fixtures/node-usage.v1.json",
        "crates/server-usage/tests/read.rs",
        "rust-toolchain.toml",
        "tools/qualification/process.rs",
        "tools/replay/Cargo.toml",
        "tools/replay/src/bin/chatgpt-status-contracts.rs",
        "tools/replay/src/bin/corpus-launcher.rs",
        "tools/replay/src/bin/diagnose-contracts.rs",
        "tools/replay/src/bin/host-control.rs",
        "tools/replay/src/bin/prompts-contracts.rs",
        "tools/replay/src/bin/routing-contracts.rs",
        "tools/replay/src/bin/support/chatgpt_status_dependencies.rs",
        "tools/replay/src/bin/support/chatgpt_status_linux_ci.rs",
        "tools/replay/src/bin/support/chatgpt_worker_auth_oracle.rs",
        "tools/replay/src/bin/support/diagnose_dependencies.rs",
        "tools/replay/src/bin/support/prompts_dependencies.rs",
        "tools/replay/src/bind.rs",
        "tools/replay/src/calendar.rs",
        "tools/replay/src/corpus.rs",
        "tools/replay/src/diff.rs",
        "tools/replay/src/http.rs",
        "tools/replay/src/lib.rs",
        "tools/replay/src/main.rs",
        "tools/replay/src/routes.rs",
        "tools/replay/src/state.rs",
        "tools/replay/src/synthetic-host-input.cjs",
        "tools/replay/src/synthetic-host-launch.cjs",
        "tools/replay/src/synthetic-host-verify.cjs",
        "tools/replay/tests/replay_e2e.rs",
    ]
}
pub fn compiled_map() -> Value {
    json!(compiled_inputs()
        .iter()
        .map(|(path, bytes)| json!({"path":path,"bytes":bytes.len(),"sha256":hash(bytes)}))
        .collect::<Vec<_>>())
}
pub fn source_binding(identity: &Value) -> Result<()> {
    let rows = field(identity, "files")
        .as_array()
        .ok_or("source identity rows")?;
    require(
        !rows.is_empty() && rows.len() <= 4096,
        "source identity row cap",
    )?;
    let mut by_path = std::collections::BTreeMap::new();
    for row in rows {
        let name = field(row, "path").as_str().ok_or("source identity path")?;
        require(
            name.len() <= 4096
                && Path::new(name)
                    .components()
                    .all(|p| matches!(p, Component::Normal(_)))
                && by_path.insert(name, row).is_none(),
            "source identity unique namespace",
        )?;
    }
    for (name, bytes) in compiled_inputs() {
        let row = by_path.get(name).ok_or("compiled source omitted")?;
        require(
            field(row, "sha256").as_str() == Some(hash(bytes).as_str())
                && field(row, "bytes").as_u64() == Some(bytes.len() as u64)
                && matches!(field(row, "mode").as_str(), Some("100644" | "100755")),
            "complete compiled source mismatch",
        )?;
    }
    Ok(())
}
// These synthetic qualification variables are supplied to both processes by the
// owning job. A root-created fresh nonce or CI run/attempt/job identifies the run.
pub fn expected_identity(root: &Path, identity: &Value) -> Result<String> {
    let env = |name| std::env::var(name).map_err(|_| "missing qualification identity");
    let job = env("NOEVIA_WORKER_AUTH_JOB")?;
    require(
        job.len() >= 16
            && job.len() <= 256
            && job
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_:".contains(&b)),
        "qualification job grammar",
    )?;
    for (key, name) in [
        ("head", "NOEVIA_WORKER_AUTH_SOURCE_HEAD"),
        ("tree", "NOEVIA_WORKER_AUTH_SOURCE_TREE"),
    ] {
        let expected = env(name)?;
        require(
            expected.len() == 40
                && expected.bytes().all(|b| b.is_ascii_hexdigit())
                && field(identity, key).as_str() == Some(expected.as_str()),
            "expected current source identity",
        )?;
    }
    require(
        env("NOEVIA_WORKER_AUTH_REFERENCE_ROOT")? == root.to_str().ok_or("reference root utf8")?,
        "expected same-job reference root",
    )?;
    Ok(job)
}
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
fn read_mode(path: &Path, cap: usize, mode: u32) -> Result<Vec<u8>> {
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
        m.is_file() && m.nlink() == 1 && m.mode() & 0o7777 == mode && m.len() <= cap as u64,
        "private reference descriptor",
    )?;
    let mut bytes = Vec::new();
    f.take((cap + 1) as u64).read_to_end(&mut bytes)?;
    require(bytes.len() <= cap, "reference cap")?;
    Ok(bytes)
}
fn read(path: &Path, cap: usize) -> Result<Vec<u8>> {
    read_mode(path, cap, 0o600)
}
fn bound(root: &Path, name: &str, sha: &Value, cap: usize) -> Result<Vec<u8>> {
    let b = read(&root.join(name), cap)?;
    require(
        sha.as_str() == Some(hash(&b).as_str()),
        "reference file hash",
    )?;
    Ok(b)
}
fn terminal_binding(binding: &Value, terminal: &Value) -> Result<()> {
    require(
        field(terminal, "outer_code") == 0 && field(terminal, "completed") == true,
        "completed worker terminal success required",
    )?;
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
        require(
            !field(binding, key).is_null() && field(terminal, key) == field(binding, key),
            "terminal binding identity",
        )?;
    }
    Ok(())
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
    let terminal: Value = serde_json::from_slice(&bound(
        root,
        "worker-outcome.json",
        field(&binding, "outcome_sha256"),
        16384,
    )?)?;
    terminal_binding(&binding, &terminal)?;
    reference_provisional(root, &binding)
}
// Internal producer validation has no exported success semantics. It is called
// before final attestation and before either the terminal outcome or binding exists.
pub(super) fn reference_provisional(root: &Path, binding: &Value) -> Result<Vec<Value>> {
    require(
        field(binding, "version") == 2
            && field(binding, "root").as_str() == root.to_str()
            && field(binding, "core_commit") == CORE
            && field(binding, "core_tree") == TREE
            && field(binding, "now") == NOW
            && field(binding, "ids") == &json!(IDS),
        "fixed worker binding",
    )?;
    require(
        field(binding, "schema_sha256").as_str() == Some(hash(SCHEMA.as_bytes()).as_str())
            && field(binding, "setup_sha256").as_str() == Some(hash(SETUP.as_bytes()).as_str())
            && field(binding, "adapter_sha256").as_str() == Some(hash(ADAPTER.as_bytes()).as_str())
            && field(binding, "test_sha256").as_str()
                == Some(
                    hash(include_bytes!(
                        "../../../../../bins/noevia-server/tests/chatgpt_worker.rs"
                    ))
                    .as_str(),
                ),
        "compiled worker source binding",
    )?;
    let identity: Value = serde_json::from_slice(&bound(
        root,
        "source-identity.json",
        field(binding, "source_identity_sha256"),
        2 * 1024 * 1024,
    )?)?;
    source_binding(&identity)?;
    require(
        field(binding, "source_head") == field(&identity, "head")
            && field(binding, "source_tree") == field(&identity, "tree")
            && field(binding, "job").as_str() == Some(expected_identity(root, &identity)?.as_str()),
        "fresh compiled source/job binding",
    )?;
    let inputs: Value = serde_json::from_slice(&bound(
        root,
        "execution-inputs.json",
        field(binding, "execution_inputs_sha256"),
        2 * 1024 * 1024,
    )?)?;
    require(
        field(&inputs, "source") == &identity
            && field(&inputs, "job") == field(binding, "job")
            && field(&inputs, "root") == field(binding, "root")
            && field(&inputs, "compiled_map") == &compiled_map()
            && field(binding, "compiled_map_sha256").as_str()
                == Some(hash(&serde_json::to_vec(&compiled_map())?).as_str())
            && field(&inputs, "coordinator_sha256") == field(binding, "tool_sha256")
            && field(&inputs, "node_sha256") == field(binding, "node_sha256"),
        "actual execution inputs binding",
    )?;
    let binary = read_mode(
        &root.join("executed-coordinator.bin"),
        128 * 1024 * 1024,
        0o700,
    )?;
    require(
        !binary.is_empty()
            && field(binding, "tool_sha256").as_str() == Some(hash(&binary).as_str()),
        "executed coordinator evidence",
    )?;
    require(
        field(field(binding, "runtime"), "version") == "v22.23.3"
            && field(field(binding, "runtime"), "abi") == "127"
            && field(field(binding, "runtime"), "platform") == "linux",
        "Linux runtime binding",
    )?;
    let cert: Value = serde_json::from_slice(&bound(
        root,
        "worker-certificate.json",
        field(binding, "certificate_sha256"),
        2 * 1024 * 1024,
    )?)?;
    let manifest = bound(
        root,
        "oracle/qualification-manifest.json",
        field(binding, "manifest_sha256"),
        2 * 1024 * 1024,
    )?;
    let manifest: Value = serde_json::from_slice(&manifest)?;
    require(
        field(&manifest, "core_commit") == CORE
            && field(&manifest, "core_tree") == TREE
            && field(&manifest, "runtime") == field(binding, "runtime")
            && field(&manifest, "node_sha256") == field(binding, "node_sha256"),
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
        field(binding, "reference_sha256"),
        1024 * 1024,
    )?;
    let rows: Vec<Value> = serde_json::from_slice(&bytes)?;
    validate(&rows)?;
    let certificate_rows: Vec<Value> = serde_json::from_value(field(&cert, "rows").clone())?;
    require(
        differences(&certificate_rows, &rows)?.is_empty()
            && field(&cert, "core_commit") == CORE
            && field(&cert, "adapter_sha256") == field(binding, "adapter_sha256"),
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

#[cfg(test)]
mod provenance_controls {
    use super::*;
    #[test]
    fn compiled_worker_and_coordinator_omission_or_staleness_refused() -> Result<()> {
        let mut rows = compiled_map().as_array().ok_or("compiled rows")?.clone();
        for row in &mut rows {
            row.as_object_mut()
                .ok_or("compiled row")?
                .insert("mode".into(), json!("100644"));
        }
        let identity = json!({"files":rows});
        source_binding(&identity)?;
        for path in [
            "bins/noevia-server/src/chatgpt_worker.rs",
            "crates/server-auth/src/lib.rs",
            "tools/replay/src/bin/chatgpt-status-contracts.rs",
            "tools/replay/src/bin/support/chatgpt_status_linux_ci.rs",
            "tools/replay/src/bin/support/chatgpt_status_dependencies.rs",
        ] {
            let mut changed = identity.clone();
            let rows = changed
                .get_mut("files")
                .and_then(Value::as_array_mut)
                .ok_or("source rows")?;
            let row = rows
                .iter_mut()
                .find(|row| field(row, "path") == path)
                .ok_or("covered source")?;
            row.as_object_mut()
                .ok_or("source row")?
                .insert("sha256".into(), json!("stale"));
            assert!(
                source_binding(&changed).is_err(),
                "stale implementation/coordinator accepted: {path}"
            );
            changed
                .get_mut("files")
                .and_then(Value::as_array_mut)
                .ok_or("source rows")?
                .retain(|row| field(row, "path") != path);
            assert!(
                source_binding(&changed).is_err(),
                "omitted implementation/coordinator accepted: {path}"
            );
        }
        Ok(())
    }
    #[test]
    fn incomplete_failed_or_unbound_terminal_refused() -> Result<()> {
        assert!(reference_provisional(Path::new("/"), &Value::Null).is_err());
        let mut binding = json!({});
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
            binding
                .as_object_mut()
                .ok_or("binding object")?
                .insert(key.into(), json!(key));
        }
        let mut terminal = binding.clone();
        terminal
            .as_object_mut()
            .ok_or("terminal object")?
            .insert("outer_code".into(), json!(0));
        terminal
            .as_object_mut()
            .ok_or("terminal object")?
            .insert("completed".into(), json!(true));
        terminal_binding(&binding, &terminal)?;
        assert!(terminal_binding(&binding, &Value::Null).is_err());
        for (key, value) in [
            ("outer_code", json!(1)),
            ("completed", json!(false)),
            ("job", json!("old-job")),
            ("tool_sha256", json!("old-executable")),
            ("source_identity_sha256", Value::Null),
        ] {
            let mut changed = terminal.clone();
            changed
                .as_object_mut()
                .ok_or("terminal object")?
                .insert(key.into(), value);
            assert!(
                terminal_binding(&binding, &changed).is_err(),
                "unqualified terminal accepted: {key}"
            );
        }
        let mut missing = binding.clone();
        missing
            .as_object_mut()
            .ok_or("binding object")?
            .remove("source_head");
        assert!(terminal_binding(&missing, &terminal).is_err());
        Ok(())
    }
}
