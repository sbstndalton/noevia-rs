//! Private CLOSED whole-route qualification. No live endpoint or arbitrary subprocess.
mod support {
    pub mod chatgpt_status_dependencies;
    pub mod chatgpt_status_linux_ci;
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("inventory-linux-dependencies") | Some("qualify-linux-factory") => {
            let args: Vec<String> = std::env::args().skip(1).take(16).collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&support::chatgpt_status_linux_ci::dispatch(&args)?)?
            );
            Ok(())
        }
        Some("copy-dependencies") => {
            let value = support::chatgpt_status_dependencies::copy()?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        Some("certify-oracle") => {
            println!("{}", serde_json::to_string_pretty(&certify_oracle()?)?);
            Ok(())
        }
        Some("materialize-oracle") => {
            let args: Vec<String> = std::env::args().skip(2).take(11).collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&support::chatgpt_status_dependencies::materialize(
                    &args
                )?)?
            );
            Ok(())
        }
        Some("capture-materialized-oracle") => {
            let args: Vec<String> = std::env::args().skip(2).take(11).collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&capture_materialized(&args)?)?
            );
            Ok(())
        }
        Some("certify-native-mutant") => {
            let name = std::env::args().nth(2).ok_or("fixed mutant name")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&certify_native_mutant(&name)?)?
            );
            Ok(())
        }
        _ => Err("fixed qualification subcommand required".into()),
    }
}

#[allow(dead_code)]
#[path = "../../../qualification/process.rs"]
mod process;
const CORE: &str = "/private/tmp/sol-routing-core010735-oracle";
const CORE_SHA: &str = "010735c7d86d30269ab3f4a351b2135faea1c8e7";
const NODE: &str = "/opt/homebrew/opt/node@22/bin/node";
const NODE_SHA: &str = "b6a0496318476702af424b12fe5f245a74c3cb93159e0c7411e6a95ad8c8d4db";
const ORACLE_DIRECTORY: &str = "/tmp/m6-chatgpt-status-oracle-fixture/seeded-v2";
// Fixed delegation only. No reimplementation of auth, device, feature, secret or
// provider policy; no index/app/server startup or provider I/O method is invoked.
const ADAPTER: &str = r#"
'use strict';
const fs=require('fs'), path=require('path');
const input=JSON.parse(fs.readFileSync(0,'utf8'));
Date.now=()=>input.now;
globalThis.fetch=()=>{throw new Error('closed fixture refuses external I/O')};
const core=input.core, authLib=require(core+'/server/auth.cjs'), deviceLib=require(core+'/server/device-auth.cjs');
const SeedDatabase=require('better-sqlite3');
const seed=new SeedDatabase(path.join(input.directory,'cowork.db'));
seed.exec(input.schema_sql); seed.exec(input.setup_sql); seed.close();
const auth=authLib.createAuth({dataDir:input.directory,publicOrigin:'',rpId:'localhost',trustProxy:true});
const db=auth.db;
const device=deviceLib.createDeviceAuth({db,audit:auth.audit,publicUser:auth.publicUser,rate:authLib.createRateLimiter(),clientAddress:r=>authLib.clientAddress(r,true),origin:()=>'',now:()=>input.now});
const requestAuth=deviceLib.createRequestAuth({enabled:()=>true,deviceAuth:device,authService:auth});
const featuresLib=require(core+'/server/features.cjs');
const secrets=require(core+'/server/secrets.cjs').createSecretStore(input.directory,{env:{}});
const oauth=require(core+'/server/chatgpt-oauth.cjs').createChatGptOAuth({db,secrets,fetchImpl:globalThis.fetch,now:()=>input.now});
const providerLib=require(core+'/server/routes/providers.cjs');
const refused=()=>{throw new Error('closed fixture refuses unrelated method')};
(async()=>{
 const cases=[];
 for(const item of input.cases){
  db.exec(item.sql);
  const features=featuresLib.createFeatures({env:{NOEVIA_FEATURE_NATIVE_CLIENT_AUTH:'true'},store:featuresLib.settingsStore(db),log:refused});
  let answer=null;
  const route=providerLib.createProviderRoutes({json:(res,status,body)=>{answer={status,body:JSON.stringify(body)};return true},readBody:refused,readJson:refused,fetchJson:refused,endpointApproved:refused,PROVIDERS:[],PROJECTS:[],modelManager:{enabled:false},currentWorkspace:refused,saveProjects:refused,registry:{saveProviders:refused,saveSharedProviders:refused,maskKey:refused},chatgptOAuth:oauth,chatgptEnabled:()=>features.enabled('chatgptOAuth')});
  const req={method:'GET',headers:item.headers,socket:{remoteAddress:'127.0.0.1',localAddress:'127.0.0.1'}};
  const authenticated=requestAuth.authenticate(req);
  const browserOnly=requestAuth.browserOnly(authenticated,'/api/providers/chatgpt','GET');
  if(authenticated&&!browserOnly) await route(req,{}, {path:'/api/providers/chatgpt',authn:authenticated,url:new URL('http://closed.invalid/api/providers/chatgpt')});
  cases.push({id:item.id,authenticated:!!authenticated,owner:authenticated?.user?.id||null,browser_only:browserOnly,route:answer,sessions:db.prepare('SELECT id_hash,last_seen_at FROM sessions ORDER BY id_hash').all(),grants:db.prepare('SELECT id,last_used_at FROM device_grants ORDER BY id').all()});
 }
 db.close();
 const modules=Object.keys(require.cache).sort();
 process.stdout.write(JSON.stringify({node:process.version,abi:process.versions.modules,cases,modules}));
})().catch(()=>{process.stderr.write('closed oracle failed\n');process.exitCode=2});
"#;
fn decimal(value: &str) -> bool {
    !value.is_empty()
        && value.split('.').count() <= 2
        && value
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}
fn classify_native(stdout: &str, stderr: &str, code: i32, name: &str) -> bool {
    let (line, id, values) = match name {
        "intercept" => (124, "M6_HTTP_ASSERT/intercept", Some(("418", "200"))),
        "upkeep" => (152, "M6_HTTP_ASSERT/upkeep-delta", Some(("1", "0"))),
        "owner" => (320, "M6_HTTP_ASSERT/owner-isolated", None),
        "readiness" => (304, "M6_HTTP_ASSERT/native-ready-and", Some(("200", "503"))),
        _ => return false,
    };
    if code != 101 || !stdout.ends_with('\n') || !stderr.ends_with('\n') {
        return false;
    }
    let prefix = "\nrunning 1 test\ntest native_status_closed_http_pipeline ... FAILED\n\nfailures:\n\nfailures:\n    native_status_closed_http_pipeline\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 2 filtered out; finished in ";
    let Some(time) = stdout
        .strip_prefix(prefix)
        .and_then(|s| s.strip_suffix("s\n\n"))
    else {
        return false;
    };
    if !decimal(time) {
        return false;
    }
    let mut lines = stderr.lines().peekable();
    if lines.peek().is_some_and(|l| l.starts_with("   Compiling "))
        && lines.next() != Some("   Compiling noevia-server v0.1.0 (/Users/sebastiandalton/noevia-work/m6-chatgpt-status-private/bins/noevia-server)") { return false; }
    let Some(finished) = lines
        .next()
        .and_then(|s| {
            s.strip_prefix("    Finished `test` profile [unoptimized + debuginfo] target(s) in ")
        })
        .and_then(|s| s.strip_suffix('s'))
    else {
        return false;
    };
    if !decimal(finished) || lines.next() != Some("     Running tests/chatgpt_status_http.rs (/tmp/m6-status-wave-b-target/debug/deps/chatgpt_status_http-4e07298b4b27b3ce)") || lines.next() != Some("") { return false; }
    let location =
        format!(") panicked at bins/noevia-server/tests/chatgpt_status_http.rs:{line}:5:");
    let Some(tid) = lines
        .next()
        .and_then(|s| s.strip_prefix("thread 'native_status_closed_http_pipeline' ("))
        .and_then(|s| s.strip_suffix(&location))
    else {
        return false;
    };
    if tid.is_empty() || !tid.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if let Some((left, right)) = values {
        if lines.next() != Some(format!("assertion `left == right` failed: {id}").as_str())
            || lines.next() != Some(format!("  left: {left}").as_str())
            || lines.next() != Some(format!(" right: {right}").as_str())
        {
            return false;
        }
    } else if lines.next() != Some(id) {
        return false;
    }
    lines.next()
        == Some("note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace")
        && lines.next()
            == Some(
                "error: test failed, to rerun pass `-p noevia-server --test chatgpt_status_http`",
            )
        && lines.next().is_none()
}
fn certify_native_mutant(name: &str) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    require(
        matches!(name, "intercept" | "upkeep" | "owner" | "readiness"),
        "known native mutant",
    )?;
    let root = std::path::Path::new("/tmp/m6-status-wave-b-evidence");
    let read = |suffix: &str, cap| read_cap(&root.join(format!("mutant-{name}.{suffix}")), cap);
    let stdout = read("stdout", 1_048_576)?;
    let stderr = read("stderr", 1_048_576)?;
    let code = read("status", 16)?;
    require(
        code == b"101\n"
            && classify_native(
                std::str::from_utf8(&stdout)?,
                std::str::from_utf8(&stderr)?,
                101,
                name,
            ),
        "whole grammar assertion-specific semantic failure",
    )?;
    let source = read("production.rs", 1_048_576)?;
    let tests = read("test.rs", 1_048_576)?;
    let binary = read("binary", 128 * 1024 * 1024)?;
    Ok(
        serde_json::json!({"mutant":name,"status":101,"stdout_sha256":hash(&stdout),"stderr_sha256":hash(&stderr),"production_sha256":hash(&source),"test_sha256":hash(&tests),"binary_sha256":hash(&binary),"qualification":"single selected test/line/column/assertion/failure inventory/terminal summary/exact Cargo footer; compile/setup/cap/deadline/duplicates/suffix rejected; local source-at-execution snapshots"}),
    )
}
fn require(ok: bool, why: &'static str) -> Result<(), Box<dyn std::error::Error>> {
    if ok {
        Ok(())
    } else {
        Err(why.into())
    }
}
fn hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}
fn read_cap(path: &std::path::Path, cap: usize) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC)
                .bits() as i32,
        )
        .open(path)?;
    let m = file.metadata()?;
    require(
        m.is_file() && m.nlink() == 1 && m.len() <= cap as u64,
        "qualification regular file cap",
    )?;
    let mut bytes = Vec::new();
    file.take((cap + 1) as u64).read_to_end(&mut bytes)?;
    require(bytes.len() <= cap, "qualification read cap")?;
    Ok(bytes)
}
fn command(
    program: &str,
    args: &[&str],
    cwd: &str,
    input: Vec<u8>,
) -> Result<process::Output, Box<dyn std::error::Error>> {
    let mut command = std::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env(
            "NODE_PATH",
            "/tmp/m6-chatgpt-status-node-dependencies/node_modules",
        );
    process::bounded(command, input, std::time::Duration::from_secs(15)).map_err(Into::into)
}
fn write_new(path: &std::path::Path, data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data)?;
    Ok(())
}
#[allow(dead_code)]
fn oracle() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    use std::{os::unix::fs::PermissionsExt, path::Path};
    let head = command("/usr/bin/git", &["rev-parse", "HEAD"], CORE, vec![])?;
    require(
        head.status.success()
            && head.stdout == format!("{CORE_SHA}\n").as_bytes()
            && head.stderr.is_empty(),
        "exact landed core HEAD",
    )?;
    let status = command("/usr/bin/git", &["status", "--porcelain"], CORE, vec![])?;
    require(
        status.status.success() && status.stdout.is_empty() && status.stderr.is_empty(),
        "immutable clean core",
    )?;
    let node = read_cap(Path::new(NODE), 128 * 1024 * 1024)?;
    require(hash(&node) == NODE_SHA, "actual fixed Node binary identity")?;
    let directory = Path::new(ORACLE_DIRECTORY);
    require(!directory.exists(), "fresh closed oracle directory")?;
    // Rust owns the key and all fixture input; no secret or external credential.
    std::fs::create_dir(directory)?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    write_new(&directory.join("secrets.key"), &[7; 32])?;
    let input = fixed_input(CORE, ORACLE_DIRECTORY);
    let input_bytes = serde_json::to_vec(&input)?;
    require(input_bytes.len() <= 8192, "fixed oracle stdin cap")?;
    write_new(
        Path::new("/tmp/m6-status-wave-b-evidence/oracle-seeded.input.json"),
        &input_bytes,
    )?;
    let out = command(NODE, &["-e", ADAPTER], CORE, input_bytes.clone())?;
    write_new(
        Path::new("/tmp/m6-status-wave-b-evidence/oracle-seeded-actual.stdout"),
        &out.stdout,
    )?;
    write_new(
        Path::new("/tmp/m6-status-wave-b-evidence/oracle-seeded-actual.stderr"),
        &out.stderr,
    )?;
    require(
        out.status.success() && out.stderr.is_empty() && out.stdout.len() <= 1_048_576,
        "closed oracle status/output",
    )?;
    certify_capture(&input_bytes, &out.stdout, "fresh checked success")
}
fn fixed_input(core: &str, directory: &str) -> serde_json::Value {
    use serde_json::json;
    let now = 1_800_000_000_000i64;
    let live = hash(b"live");
    let idle = hash(b"idle");
    let device = hash(b"nva_device");
    let setup=format!("INSERT INTO users(id,username,username_norm,display_name,role,password_hash,webauthn_user_id,created_at,updated_at) VALUES('u','Alpha','alpha','Alpha','admin','x','w',1,1),('other','Other','other','Other','member','x','z',2,2); INSERT INTO sessions VALUES('{live}','u','c',1,{}, {},'ua','ip'),('{idle}','u','c',1,{}, {},'ua','ip');",now-1000,now+99999,now-604800001,now+99999);
    let grant=format!("INSERT INTO device_grants VALUES('g','u','Mac',1,{}, {},'ip','ua'); INSERT INTO device_tokens VALUES('{device}','g','access',1,{},NULL,NULL);",now-60000,now+99999,now+99999);
    let cases = json!([
        {"id":"live-disconnected","headers":{"cookie":"cowork_session=live"},"sql":format!("{grant} INSERT INTO settings VALUES('feature:chatgptOAuth','true');")},
        {"id":"idle-upkeep","headers":{"cookie":"cowork_session=idle"},"sql":""},
        {"id":"unauthorized","headers":{},"sql":""},
        {"id":"device-browser-only","headers":{"authorization":"Bearer nva_device"},"sql":""},
        {"id":"mixed-credential","headers":{"cookie":"cowork_session=live","authorization":"Bearer nva_device"},"sql":""},
        {"id":"owner-isolated","headers":{"cookie":"cowork_session=live"},"sql":"INSERT INTO chatgpt_oauth_tokens VALUES('u','{\"email\":\"alice@example.invalid\",\"plan\":\"synthetic-alpha\"}','connected',1),('other','{\"email\":\"other@example.invalid\",\"plan\":\"synthetic-other\"}','connected',1);"},
        {"id":"product-off-before-schema","headers":{"cookie":"cowork_session=live"},"sql":"UPDATE settings SET value='false' WHERE key='feature:chatgptOAuth'; DROP TABLE chatgpt_oauth_tokens;"}
    ]);
    let schema = include_str!("../../../../crates/server-store/tests/fixtures/node-schema.sql");
    json!({"core":core,"directory":directory,"now":now,"schema_sql":schema,"schema_sha256":hash(schema.as_bytes()),"setup_sql":setup,"cases":cases})
}
/// Future authoritative closed capture. No local invocation is authorized yet.
/// One freshly materialized topology replaces inherited parent module resolution.
#[allow(clippy::indexing_slicing)] // serde_json string-key indexing yields Null, never panics.
fn capture_materialized(args: &[String]) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    use std::{path::Path, time::Duration};
    let manifest = support::chatgpt_status_dependencies::materialize(args)?;
    let root = Path::new(manifest["output"].as_str().ok_or("owned topology")?);
    let canonical = std::fs::canonicalize(root)?;
    let node = manifest["node"].as_str().ok_or("recorded runtime")?;
    require(
        hash(&read_cap(Path::new(node), 128 * 1024 * 1024)?) == manifest["node_sha256"],
        "runtime unchanged before capture",
    )?;
    let input = read_cap(&root.join("qualification-input.json"), 8192)?;
    require(
        hash(&input) == manifest["input_sha256"]
            && hash(ADAPTER.as_bytes()) == manifest["adapter_sha256"],
        "pre-execution fixed input and adapter",
    )?;
    let rows = manifest["files"]
        .as_array()
        .ok_or("pre-execution source inventory")?;
    require(rows.len() <= 5120, "pre-execution inventory cap")?;
    // Recheck every materialized source before execution, not merely loaded paths.
    for row in rows {
        let name = row["path"].as_str().ok_or("recorded source path")?;
        let bytes = read_cap(&root.join(name), 16 * 1024 * 1024)?;
        require(
            hash(&bytes) == row["sha256"] && bytes.len() == row["bytes"],
            "whole topology unchanged before execution",
        )?;
    }
    let mut command = std::process::Command::new(node);
    command
        .args(["-e", ADAPTER])
        .current_dir(root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin");
    let result = process::bounded(command, input, Duration::from_secs(15));
    let result = match result {
        Ok(out) => out,
        Err(error) => {
            write_new(&root.join("capture-process-error.txt"), error.as_bytes())?;
            return Err("bounded capture setup/cap/deadline failure; retained separately".into());
        }
    };
    write_new(&root.join("capture.stdout"), &result.stdout)?;
    write_new(&root.join("capture.stderr"), &result.stderr)?;
    write_new(
        &root.join("capture.status"),
        format!("{:?}\n", result.status.code()).as_bytes(),
    )?;
    require(
        result.status.success() && result.stderr.is_empty() && result.stdout.len() <= 1_048_576,
        "captured oracle success and empty stderr",
    )?;
    let actual: serde_json::Value = serde_json::from_slice(&result.stdout)?;
    require(
        actual["node"] == "v22.23.3" && actual["abi"] == "127",
        "captured runtime dialect",
    )?;
    let modules = actual["modules"]
        .as_array()
        .ok_or("loaded file inventory")?;
    require(modules.len() <= 512, "loaded file count")?;
    let mut unique = std::collections::BTreeSet::new();
    let mut bindings = Vec::new();
    for module in modules {
        let name = module.as_str().ok_or("loaded filename")?;
        require(
            name.len() <= 4096 && unique.insert(name),
            "bounded unique loaded path",
        )?;
        let relative = Path::new(name)
            .strip_prefix(&canonical)?
            .to_str()
            .ok_or("loaded relative utf8")?;
        let row = rows
            .iter()
            .find(|r| r["path"] == relative)
            .ok_or("loaded path absent from pre-execution private topology")?;
        let bytes = read_cap(Path::new(name), 16 * 1024 * 1024)?;
        require(
            hash(&bytes) == row["sha256"] && bytes.len() == row["bytes"],
            "loaded file matches pre-execution identity",
        )?;
        bindings.push(row.clone());
    }
    let cases = actual["cases"].as_array().ok_or("actual cases")?;
    let fixed: serde_json::Value =
        serde_json::from_slice(&read_cap(&root.join("qualification-input.json"), 8192)?)?;
    require(
        cases.len() == 7
            && cases
                .iter()
                .zip(fixed["cases"].as_array().ok_or("fixed cases")?)
                .all(|(a, b)| a["id"] == b["id"]),
        "exact ordered seven cases",
    )?;
    let certificate = serde_json::json!({"manifest":manifest,"stdout_sha256":hash(&result.stdout),"stderr_sha256":hash(&result.stderr),"raw_exit_code":result.status.code(),"loaded_files":bindings,"cases":cases,"qualification":"fresh closed immutable factory delegation with current supplied inventory binding; no registry integrity, hidden runtime dependencies, network/provider behavior, hard RSS/OOM or kernel deadline proof"});
    write_new(
        &root.join("capture-certificate.json"),
        &serde_json::to_vec_pretty(&certificate)?,
    )?;
    Ok(certificate)
}
#[allow(clippy::indexing_slicing)] // Only serde_json string-key indexing.
fn certify_capture(
    input_bytes: &[u8],
    stdout: &[u8],
    origin: &str,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    use serde_json::json;
    use std::path::Path;
    let input: serde_json::Value = serde_json::from_slice(input_bytes)?;
    let actual: serde_json::Value = serde_json::from_slice(stdout)?;
    require(
        actual["node"] == "v22.23.3" && actual["abi"] == "127",
        "fixed Node version/native ABI",
    )?;
    let modules = actual["modules"]
        .as_array()
        .ok_or("oracle loaded modules")?;
    require(modules.len() <= 512, "loaded module inventory cap")?;
    let copied: serde_json::Value = serde_json::from_slice(&read_cap(
        Path::new("/tmp/m6-status-wave-b-evidence/dependency-copy.stdout"),
        1_048_576,
    )?)?;
    let dependency_rows = copied["files"].as_array().ok_or("copied files")?;
    let verified_copy = support::chatgpt_status_dependencies::verify(&copied)?;
    let canonical_dependencies = std::fs::canonicalize("/tmp/m6-chatgpt-status-node-dependencies")?;
    require(
        canonical_dependencies == Path::new("/private/tmp/m6-chatgpt-status-node-dependencies"),
        "only fixed canonical tmp alias",
    )?;
    let mut bindings = Vec::new();
    for module in modules {
        let name = module.as_str().ok_or("module path")?;
        require(name.len() <= 4096, "loaded module path cap")?;
        let path = Path::new(name);
        let bytes = read_cap(path, 16 * 1024 * 1024)?;
        let sha = hash(&bytes);
        if let Ok(relative) = path.strip_prefix(CORE) {
            let relative = relative.to_str().ok_or("core module encoding")?;
            require(
                relative.starts_with("server/") && !relative.contains(".."),
                "fixed core loaded module path",
            )?;
            let spec = format!("{CORE_SHA}:{relative}");
            let blob = command("/usr/bin/git", &["show", &spec], CORE, vec![])?;
            require(
                blob.status.success() && blob.stderr.is_empty() && blob.stdout == bytes,
                "loaded actual source equals landed Git blob",
            )?;
            let oid = command("/usr/bin/git", &["rev-parse", &spec], CORE, vec![])?;
            require(
                oid.status.success() && oid.stderr.is_empty(),
                "core Git blob identity",
            )?;
            bindings.push(json!({"path":relative,"sha256":sha,"git_blob":String::from_utf8(oid.stdout)?.trim()}));
        } else {
            let relative = path
                .strip_prefix(&canonical_dependencies)?
                .to_str()
                .ok_or("dependency module encoding")?;
            require(
                dependency_rows.iter().any(|r| {
                    r["path"] == relative && r["sha256"] == sha && r["bytes"] == bytes.len()
                }),
                "loaded dependency belongs to validated private copy",
            )?;
        }
    }
    // Bind the actual composition source too; it is inspected, never executed.
    let index = read_cap(&Path::new(CORE).join("server/index.cjs"), 1_048_576)?;
    let blob = command(
        "/usr/bin/git",
        &["show", &format!("{CORE_SHA}:server/index.cjs")],
        CORE,
        vec![],
    )?;
    require(
        blob.status.success() && blob.stdout == index && blob.stderr.is_empty(),
        "actual GET caller composition source",
    )?;
    let value = json!({"version":1,"core_commit":CORE_SHA,"core_tree":"345adfa12da083eb5888ec232dc2d86b9b0383a5","node":{"path":NODE,"sha256":NODE_SHA,"version":"v22.23.3","abi":"127"},"dependency_copy_sha256":hash(&read_cap(Path::new("/tmp/m6-status-wave-b-evidence/dependency-copy.stdout"),1_048_576)?),"verified_copy":verified_copy,"adapter_sha256":hash(ADAPTER.as_bytes()),"input":input,"input_sha256":hash(input_bytes),"actual":actual,"actual_stdout_sha256":hash(stdout),"core_modules":bindings,"caller_index_sha256":hash(&index),"capture_status_origin":origin,"capture_tool_status":1,"qualification":"historical actual immutable methods only; Node success source-inferred from checked success before post-binding failure, no independent raw Node exit capture; replay not fresh execution; index gate composition source-bound, not full Node server execution; separate product feature refresh/ownership handoff and authoritative future CI held; no provider I/O"});
    let bytes = serde_json::to_vec_pretty(&value)?;
    require(bytes.len() <= 1_048_576, "fixed fixture output cap")?;
    write_new(
        Path::new("contracts/http/chatgpt-status-owned.v1.json"),
        &bytes,
    )?;
    Ok(
        json!({"cases":7,"fixture":"contracts/http/chatgpt-status-owned.v1.json","sha256":hash(&bytes),"core":CORE_SHA,"oracle_actual_status":"source-inferred historical success; not raw exit capture","replay_only":true}),
    )
}
fn certify_oracle() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    use std::path::Path;
    let bindings = [
        (
            "oracle-seeded.input.json",
            "3a22e6e976f690444e1304e71c591179132f050a975651d57977572788a63b53",
            8192,
        ),
        (
            "oracle-seeded-actual.stdout",
            "55aeb34bbaa8568f287ef8dc5d51a90d6640ecf84a5b68e4fd6804c11fbdd19c",
            1_048_576,
        ),
        (
            "oracle-seeded-actual.stderr",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            4096,
        ),
        (
            "oracle-tool-seeded-source.rs",
            "7898c7aa5953b378d0c8c7e434e5c705c925f653c36b8fae7e2baeb25adbdb88",
            65536,
        ),
        (
            "oracle-seeded-tool-binary",
            "d9fa571211e45f00a743f4b5c2823def44e0fdd7e7073b11a95cc1956527077c",
            16_777_216,
        ),
        (
            "dependency-copy.stdout",
            "ab2f17128e6e4dd08f24bb6ae07d10b6ec8afb551078a0cbfcf7c793ed5575b9",
            1_048_576,
        ),
    ];
    let mut bytes = Vec::new();
    for (name, sha, cap) in bindings {
        let value = read_cap(&Path::new("/tmp/m6-status-wave-b-evidence").join(name), cap)?;
        require(hash(&value) == sha, "exact historical replay identity")?;
        bytes.push(value);
    }
    let input = bytes.first().ok_or("input capture")?;
    let stdout = bytes.get(1).ok_or("stdout capture")?;
    require(
        bytes.get(2).is_some_and(Vec::is_empty),
        "empty captured stderr",
    )?;
    let source = std::str::from_utf8(bytes.get(3).ok_or("source capture")?)?;
    require(
        source.contains(ADAPTER),
        "exact executed adapter matches replay adapter",
    )?;
    require(
        hash(&read_cap(Path::new(NODE), 128 * 1024 * 1024)?) == NODE_SHA,
        "fixed actual runtime identity retained",
    )?;
    certify_capture(
        input,
        stdout,
        "SOURCE-INFERRED Node success; original Rust tool post-binding status1; replay-only",
    )
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod classifier_controls {
    #[test]
    fn strict_native_output_grammar() {
        let out = "\nrunning 1 test\ntest native_status_closed_http_pipeline ... FAILED\n\nfailures:\n\nfailures:\n    native_status_closed_http_pipeline\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 2 filtered out; finished in 0.04s\n\n";
        let err = "    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.12s\n     Running tests/chatgpt_status_http.rs (/tmp/m6-status-wave-b-target/debug/deps/chatgpt_status_http-4e07298b4b27b3ce)\n\nthread 'native_status_closed_http_pipeline' (123) panicked at bins/noevia-server/tests/chatgpt_status_http.rs:124:5:\nassertion `left == right` failed: M6_HTTP_ASSERT/intercept\n  left: 418\n right: 200\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\nerror: test failed, to rerun pass `-p noevia-server --test chatgpt_status_http`\n";
        assert!(super::classify_native(out, err, 101, "intercept"));
        for bad in [
            err.replace(":124:5:", ":124:1:"),
            err.replace(":124:5:", ":125:5:"),
            err.replace("M6_HTTP_ASSERT/intercept", "M6_HTTP_ASSERT/upkeep"),
            format!("{err}suffix\n"),
            format!("error: compiler failed\n{err}"),
            format!("{err}{err}"),
            err.replace("  left: 418", "  left: 503"),
            err.replace(
                "error: test failed, to rerun pass `-p noevia-server --test chatgpt_status_http`\n",
                "",
            ),
        ] {
            assert!(!super::classify_native(out, &bad, 101, "intercept"));
        }
        for bad in [
            format!("{out}suffix\n"),
            format!("{out}{out}"),
            out.replace("1 failed", "2 failed"),
            out.replace("0.04s", "NaNs"),
            out.replace(
                "    native_status_closed_http_pipeline",
                "    unrelated_test",
            ),
        ] {
            assert!(!super::classify_native(&bad, err, 101, "intercept"));
        }
        for code in [0, 1, 102] {
            assert!(!super::classify_native(out, err, code, "intercept"));
        }
    }
}
