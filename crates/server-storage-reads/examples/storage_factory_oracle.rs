//! Stage A source artifact. No production integration or runtime qualification.
#[path = "support/storage_factory_capture.rs"]
pub mod process;
#[path = "support/storage_factory_topology.rs"]
pub mod topology;

/// Only fixed factory delegation. Fixture SQL, typed rows, keys, expected values,
/// caps, SQL deltas and verdicts belong to Rust. Never import index.cjs.
/// This adapter takes an already prepared private synthetic directory.
pub const FACTORY_ADAPTER: &str = r#"'use strict';
const fs = require('node:fs');
const input = JSON.parse(fs.readFileSync(0, 'utf8'));
Date.now=()=>input.now;
const {createSecretStore} = require('./server/secrets.cjs');
const {createAuth} = require('./server/auth.cjs');
const {createDeviceAuth, createRequestAuth} = require('./server/device-auth.cjs');
const {createStorageRoutes} = require('./server/routes/storage.cjs');
const http = require('./server/http.cjs');
const envelope = require('./server/secret-envelope.cjs');
const snapshot=(auth)=>input.stateSQL.map(sql=>{try{const q=auth.db.prepare(sql);return {columns:q.columns().map(v=>v.name),rows:q.all().map(row=>q.columns().map(({name})=>{const v=row[name];return v===null?{t:'null',v:null}:Buffer.isBuffer(v)?{t:'blob',v:v.toString('hex')}:typeof v==='string'?{t:'text',v}:Number.isInteger(v)?{t:'integer',v}:{t:'real',v};}))};}catch(e){return {unavailable:true,code:e.code??null};}});
const errorRecord=e=>({name:String(e?.name??'unknown').slice(0,80),code:e?.code===undefined?null:String(e.code).slice(0,80),reason:e?.reason===undefined?null:String(e.reason).slice(0,80),message:String(e?.message??'unknown').slice(0,512)});
const rowEvidence=row=>({presence:row===undefined?'row-missing':Object.hasOwn(row,'secret')?'present':'property-missing',row:row===undefined?null:Object.fromEntries(Object.entries(row).map(([k,v])=>[k,v===null?{t:'null',v:null}:Buffer.isBuffer(v)?{t:'blob',v:v.toString('hex')}:typeof v==='string'?{t:'text',v}:Number.isInteger(v)?{t:'integer',v}:{t:'real',v}]))});
(async()=>{const outputs=[];for(const c of input.cases){let auth;let phase='keys';let before=null;let bootAfter=null;try {
 const secrets = createSecretStore(c.dataDir,{env:c.env});
 phase='boot';auth = createAuth({dataDir:c.dataDir,publicOrigin:'https://owner.invalid',rpId:'owner.invalid',secrets,trustProxy:true});
 bootAfter=snapshot(auth);
 if(c.shapeSQL)auth.db.exec(c.shapeSQL);
 if(c.insertSQL)auth.db.prepare(c.insertSQL).run(c.cells.map(v=>v.t==='blob'?Buffer.from(v.v,'hex'):v.v));
 if(c.foreignSQL)auth.db.prepare(c.foreignSQL).run(c.foreignCells.map(v=>v.v));
 before=snapshot(auth);
 phase='factory';const device = createDeviceAuth({db:auth.db,audit:auth.audit,publicUser:auth.publicUser,origin:()=>auth.origin,now:()=>input.now,addressesTrusted:true});
 const requestAuth = createRequestAuth({enabled:()=>c.deviceEnabled,deviceAuth:device,authService:auth});
 const req={method:c.method,headers:c.headers};
 const authn=requestAuth.authenticate(req);
 const rawCsrf=authn?requestAuth.csrfValid(req,authn):null;
 const components={owner:authn?.user.id??null,role:authn?.user.role??null,device:authn?!!authn.device:null,originValid:auth.originValid(req),csrfValid:authn?Boolean(rawCsrf):null,browserOnly:authn?requestAuth.browserOnly(authn,c.path,c.method):null};
 const factoryOwner=c.gate?null:c.owner;
 phase='projection';
 const storage=factoryOwner===null?null:auth.getStorage(factoryOwner,true);
 let metadata=null;let falseStorage=null;let falseBefore=null;let falseAfter=null;let falseStored=null;let falseStoredEvidence=null;let falseStoredPlain=null;let falseCurrentOpen=null;let falseForeignOpen=null;
 if(c.public){
  phase='metadata';
  if(!authn)throw new Error('synthetic authenticated metadata fixture refused');
  const falseSecrets=createSecretStore(c.falseDir,{env:c.env});
  const falseAuth=createAuth({dataDir:c.falseDir,publicOrigin:'https://owner.invalid',rpId:'owner.invalid',secrets:falseSecrets,trustProxy:true});
  try{falseBefore=snapshot(falseAuth);falseStorage=falseAuth.getStorage(c.owner,false);
  const res={writeHead(status,headers){this.status=status;this.headers=headers;},end(body){metadata={status:this.status,headers:this.headers,body:JSON.parse(body)};}};
  const routes=createStorageRoutes({json:http.json,authService:falseAuth});
  await routes({method:'GET'},res,{path:'/api/integrations/storage',authn});
  falseAfter=snapshot(falseAuth);const falseRow=falseAuth.db.prepare(c.evidenceSQL).get(c.owner);falseStoredEvidence=rowEvidence(falseRow);falseStored=falseRow?.secret;
  if(falseStored!==undefined){falseStoredPlain=falseSecrets.decrypt(falseStored,c.owner);const key=fs.readFileSync(falseSecrets.keyFile);try{falseCurrentOpen=envelope.open(key,null,falseStored,c.owner);}catch(e){falseCurrentOpen={error:errorRecord(e)};}try{falseForeignOpen=envelope.open(key,null,falseStored,'foreign');}catch(e){falseForeignOpen={error:errorRecord(e)};}}
  }finally{falseAuth.db.close();}
 }
 phase='evidence';const after=snapshot(auth);
 const storedRow=auth.db.prepare(c.evidenceSQL).get(factoryOwner);const storedEvidence=rowEvidence(storedRow);const stored=storedRow?.secret;
 let storedPlain=null;try{if(stored!==undefined)storedPlain=secrets.decrypt(stored,factoryOwner);}catch{storedPlain=null;}
 let currentOpen=null;let foreignOpen=null;
 if(stored!==undefined){const current=fs.readFileSync(secrets.keyFile);try{currentOpen=envelope.open(current,null,stored,factoryOwner);}catch(e){currentOpen={error:errorRecord(e)};}try{foreignOpen=envelope.open(current,null,stored,'foreign');}catch(e){foreignOpen={error:errorRecord(e)};}}
 outputs.push({id:c.id,owner:factoryOwner,components,rawCsrf:{type:typeof rawCsrf,value:rawCsrf},storage,metadata,falseStorage,falseBefore,falseAfter,falseStored,falseStoredEvidence,falseStoredPlain,falseCurrentOpen,falseForeignOpen,bootAfter,before,after,stored,storedEvidence,storedPlain,currentOpen,foreignOpen});
}catch(e){outputs.push({id:c.id,exceptionPhase:phase,error:errorRecord(e),bootAfter,before,after:auth?snapshot(auth):null});}finally{if(auth)auth.db.close();}}
 const modules=Object.keys(require.cache).map(p=>fs.realpathSync(p)).sort();
 process.stdout.write(JSON.stringify({runtime:{version:process.version,abi:process.versions.modules,platform:process.platform,arch:process.arch},outputs,modules}));
})().catch(()=>{process.exitCode=1;});
"#;

/// Fixed metadata/native loading preflight. No hashing/password/account methods.
pub const PREFLIGHT: &str = r#"'use strict';
const fs=require('node:fs');const moduleAPI=require('node:module');
const Database=require('better-sqlite3');const db=new Database(':memory:');db.close();
require('@node-rs/argon2');
const modules=Object.keys(require.cache).map(p=>fs.realpathSync(p)).sort();
const sharedObjects=process.report.getReport().sharedObjects;
const cryptoLibraries=sharedObjects.filter(p=>p.endsWith('/libnode.127.dylib')).map(p=>fs.realpathSync(p));
process.stdout.write(JSON.stringify({runtime:{version:process.version,abi:process.versions.modules,platform:process.platform,arch:process.arch},modules,native:modules.filter(p=>p.endsWith('.node')),builtins:moduleAPI.builtinModules.slice().sort(),loadedBuiltins:process.moduleLoadList.filter(p=>p.startsWith('NativeModule ')).sort(),sharedObjects,cryptoLibraries,ancestry:'OS/dylib ancestry not certified'}));
"#;

fn preflight() -> topology::Result<()> {
    use serde_json::{json, Value};
    use std::{
        collections::BTreeSet,
        path::{Path, PathBuf},
        process::Command,
        time::Duration,
    };
    let manifest = topology::approved_manifest()?;
    let root = Path::new(topology::PREFLIGHT_ROOT);
    topology::directory(root)?;
    let mut cmd = Command::new(topology::NODE);
    cmd.current_dir(topology::ROOT)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .env("SECRET_ENVELOPE_IMPL", "js")
        .args(["--no-warnings", "-e", PREFLIGHT]);
    topology::put(
        &root.join("command.json"),
        &topology::json_bytes(
            &json!({"program":topology::NODE,"args":["--no-warnings","-e",PREFLIGHT],"cwd":topology::ROOT,"env":{"PATH":"/usr/bin:/bin","HOME":root,"LC_ALL":"C","TZ":"UTC","SECRET_ENVELOPE_IMPL":"js"},"node_sha256":topology::NODE_SHA,"manifest_sha256":topology::APPROVED_MANIFEST_SHA}),
            65536,
        )?,
    )?;
    let o = process::bounded(cmd, vec![], Duration::from_secs(30));
    topology::retain_capture(root, "native-runtime", &o)?;
    topology::require(
        o.failure.is_none() && o.status.is_some_and(|s| s.success()) && o.stderr.is_empty(),
        "native/runtime preflight fault; no retry",
    )?;
    let value: Value = serde_json::from_slice(&o.stdout).map_err(|_| "preflight JSON")?;
    topology::require(
        value.get("runtime")
            == Some(&json!({"version":"v22.23.3","abi":"127","platform":"darwin","arch":"arm64"})),
        "preflight runtime ABI",
    )?;
    let files = manifest
        .get("files")
        .and_then(Value::as_array)
        .ok_or("inventory")?
        .iter()
        .map(|v| topology::relative(topology::string(v, "path")?))
        .collect::<topology::Result<BTreeSet<PathBuf>>>()?;
    let canonical = Path::new(topology::string(&manifest, "canonical_root")?);
    for name in ["modules", "native"] {
        let rows = value
            .get(name)
            .and_then(Value::as_array)
            .ok_or("preflight module inventory")?;
        topology::require(rows.len() <= 5120, "preflight module cap")?;
        for row in rows {
            topology::require(
                topology::contained_module(
                    canonical,
                    row.as_str().ok_or("preflight path")?,
                    &files,
                ),
                "preflight independent native origin",
            )?;
        }
    }
    topology::require(
        value
            .get("native")
            .and_then(Value::as_array)
            .is_some_and(|v| v.len() == 2),
        "actual SQLite and Argon2 native loads",
    )?;
    topology::require(
        value.get("cryptoLibraries") == Some(&json!([topology::NODE_CRYPTO_LIBRARY])),
        "observed fixed native crypto error library",
    )?;
    topology::verify_materialized(&manifest)?;
    topology::put(
        &root.join("preflight.json"),
        &topology::json_bytes(
            &json!({"manifest_sha256":topology::APPROVED_MANIFEST_SHA,"node_sha256":topology::NODE_SHA,"script_sha256":topology::hash(PREFLIGHT.as_bytes()),"stdout_sha256":topology::hash(&o.stdout),"stderr_sha256":topology::hash(&o.stderr),"status_sha256":topology::hash(&topology::read(&root.join("native-runtime.status.json"),4096)?),"observation":value,"limits":"logical cooperative; no OS/dylib ancestry/RSS/OOM/kernel certificate"}),
            2 * 1024 * 1024,
        )?,
    )?;
    Ok(())
}

/// Fixed synthetic matrix; no invocation is authorized in Stage A.
/// No invocation is authorized in Stage A. A later exact binary grant is required.
fn run() -> topology::Result<()> {
    use serde_json::{json, Value};
    use std::{
        collections::BTreeSet,
        path::{Path, PathBuf},
        process::Command,
        time::{Duration, Instant},
    };
    let started = Instant::now();
    let manifest = topology::approved_manifest()?;
    topology::require(
        topology::APPROVED_PREFLIGHT_SHA.len() == 64,
        "root preflight approval held",
    )?;
    let preflight = topology::read(
        &Path::new(topology::PREFLIGHT_ROOT).join("preflight.json"),
        2 * 1024 * 1024,
    )?;
    topology::require(
        topology::hash(&preflight) == topology::APPROVED_PREFLIGHT_SHA,
        "root approved preflight",
    )?;
    let p: Value = serde_json::from_slice(&preflight).map_err(|_| "preflight JSON")?;
    topology::require(
        p.get("manifest_sha256") == Some(&json!(topology::APPROVED_MANIFEST_SHA))
            && p.get("node_sha256") == Some(&json!(topology::NODE_SHA))
            && p.get("script_sha256") == Some(&json!(topology::hash(PREFLIGHT.as_bytes()))),
        "preflight source identity",
    )?;
    for (field, file, cap) in [
        ("stdout_sha256", "native-runtime.stdout", 8 * 1024 * 1024),
        ("stderr_sha256", "native-runtime.stderr", 65536),
        ("status_sha256", "native-runtime.status.json", 4096),
    ] {
        topology::require(
            p.get(field)
                == Some(&json!(topology::hash(&topology::read(
                    &Path::new(topology::PREFLIGHT_ROOT).join(file),
                    cap
                )?))),
            "preflight retained raw binding",
        )?;
    }
    let root = Path::new(topology::CAPTURE_ROOT);
    topology::directory(root)?;
    topology::directory(&root.join("fixtures"))?;
    let files: BTreeSet<PathBuf> = manifest
        .get("files")
        .and_then(Value::as_array)
        .ok_or("inventory")?
        .iter()
        .map(|v| topology::relative(topology::string(v, "path")?))
        .collect::<topology::Result<_>>()?;
    let canonical = Path::new(topology::string(&manifest, "canonical_root")?);
    let all = topology::cases()?;
    let selected = all.as_slice();
    let mut captures = Vec::new();
    for (shard_index, shard) in selected.chunks(14).enumerate() {
        topology::require(
            started.elapsed() < Duration::from_secs(600),
            "whole cooperative deadline",
        )?;
        let mut inputs = Vec::new();
        let mut candidates = Vec::new();
        for (case_index, case) in shard.iter().enumerate() {
            let stem = format!("{shard_index:02}-{case_index:02}");
            let node_dir = root.join("fixtures").join(format!("node-{stem}"));
            let rust_dir = root.join("fixtures").join(format!("rust-{stem}"));
            topology::fixture(&node_dir, case, false)?;
            topology::fixture(&rust_dir, case, true)?;
            let false_dir = root.join("fixtures").join(format!("false-{stem}"));
            if case.get("public").and_then(Value::as_bool) == Some(true) {
                topology::fixture(&false_dir, case, true)?;
            }
            let boot_before = topology::state(&node_dir)?;
            let candidate = topology::candidate(&rust_dir, case)?;
            let cells = topology::row_columns(case)?
                .into_iter()
                .map(|field| {
                    case.get("row")
                        .and_then(|r| r.get(field))
                        .cloned()
                        .ok_or("row cell".into())
                })
                .collect::<topology::Result<Vec<_>>>()?;
            let foreign = topology::foreign_case(case)?;
            let foreign_sql = foreign
                .as_ref()
                .map(topology::insert_sql)
                .transpose()?
                .unwrap_or_default();
            let foreign_cells = if let Some(f) = foreign.as_ref() {
                topology::row_columns(f)?
                    .into_iter()
                    .map(|field| {
                        f.get("row")
                            .and_then(|r| r.get(field))
                            .cloned()
                            .ok_or("foreign cell".into())
                    })
                    .collect::<topology::Result<Vec<_>>>()?
            } else {
                vec![]
            };
            inputs.push(json!({"id":case.get("id"),"dataDir":node_dir,"falseDir":false_dir,"owner":case.get("owner"),"gate":topology::string(case,"lane")?=="gate","env":{"SECRETS_KEY_PREVIOUS":case.get("previous_env").and_then(Value::as_str).unwrap_or("")},"deviceEnabled":case.get("device_enabled"),"headers":case.get("headers"),"method":case.get("method"),"path":case.get("path"),"public":case.get("public"),"shapeSQL":topology::shape_sql(topology::string(case,"shape")?)?,"insertSQL":topology::insert_sql(case)?,"cells":cells,"foreignSQL":foreign_sql,"foreignCells":foreign_cells,"evidenceSQL":topology::STORAGE_EVIDENCE_SQL,"schemaEvidenceContract":topology::storage_schema_evidence_contract(case)?}));
            candidates
                .push(json!({"id":case.get("id"),"candidate":candidate,"node_dir":node_dir,"boot_before":boot_before}));
        }
        let input = topology::json_bytes(
            &json!({"now":topology::NOW,"stateSQL":topology::STATE_SQL,"cases":inputs}),
            4 * 1024 * 1024,
        )?;
        topology::put(
            &root.join(format!("capture-input-{shard_index:02}.json")),
            &input,
        )?;
        let mut cmd = Command::new(topology::NODE);
        cmd.current_dir(topology::ROOT)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root)
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .env("SECRET_ENVELOPE_IMPL", "js")
            .args(["--no-warnings", "qualification-adapter.cjs"]);
        topology::put(
            &root.join(format!("command-{shard_index:02}.json")),
            &topology::json_bytes(
                &json!({"program":topology::NODE,"args":["--no-warnings","qualification-adapter.cjs"],"cwd":topology::ROOT,"env":{"PATH":"/usr/bin:/bin","HOME":root,"LC_ALL":"C","TZ":"UTC","SECRET_ENVELOPE_IMPL":"js"},"input_sha256":topology::hash(&input),"node_sha256":topology::NODE_SHA,"adapter_sha256":topology::hash(FACTORY_ADAPTER.as_bytes())}),
                65536,
            )?,
        )?;
        topology::verify_materialized(&manifest)?;
        let output = process::bounded(cmd, input, Duration::from_secs(30));
        topology::retain_capture(root, &format!("capture-{shard_index:02}"), &output)?;
        topology::put(
            &root.join(format!("stdout-{shard_index:02}.raw")),
            &output.stdout,
        )?;
        topology::put(
            &root.join(format!("stderr-{shard_index:02}.raw")),
            &output.stderr,
        )?;
        topology::put(
            &root.join(format!("status-{shard_index:02}.json")),
            &topology::json_bytes(
                &json!({"code":output.status.and_then(|s|s.code()),"success":output.status.map(|s|s.success())}),
                1024,
            )?,
        )?;
        topology::require(
            output.failure.is_none() && output.status.is_some_and(|s| s.success()),
            "actual Node process failed; no retry",
        )?;
        qualify_diagnostics(shard, &candidates, &output.stderr)?;
        let oracle: Value = serde_json::from_slice(&output.stdout).map_err(|_| "actual JSON")?;
        topology::require(
            oracle.get("runtime")
                == Some(
                    &json!({"version":"v22.23.3","abi":"127","platform":"darwin","arch":"arm64"}),
                ),
            "actual runtime dialect",
        )?;
        let modules = oracle
            .get("modules")
            .and_then(Value::as_array)
            .ok_or("actual modules")?;
        topology::require(modules.len() <= 5120, "module cap")?;
        for module in modules {
            topology::require(
                topology::contained_module(
                    canonical,
                    module.as_str().ok_or("module text")?,
                    &files,
                ),
                "one-origin modules",
            )?;
        }
        let outputs = oracle
            .get("outputs")
            .and_then(Value::as_array)
            .ok_or("factory outputs")?;
        topology::require(outputs.len() == shard.len(), "exact outputs")?;
        let candidate_bytes = topology::json_bytes(&json!(candidates), 8 * 1024 * 1024)?;
        topology::put(
            &root.join(format!("candidates-{shard_index:02}.json")),
            &candidate_bytes,
        )?;
        captures.push(json!({"shard":shard_index,"stdout_sha256":topology::hash(&output.stdout),"stderr_sha256":topology::hash(&output.stderr),"status_sha256":topology::hash(&topology::read(&root.join(format!("capture-{shard_index:02}.status.json")),4096)?),"candidate_sha256":topology::hash(&candidate_bytes)}));
    }
    let report = json!({"actual_core":topology::CORE,"candidate_production":topology::CANDIDATE,"source_cases":126,"manifest_sha256":topology::APPROVED_MANIFEST_SHA,"preflight_sha256":topology::APPROVED_PREFLIGHT_SHA,"captures":captures,"qualification":"capture complete; no comparison certificate"});
    topology::put(
        &root.join("capture-complete.json"),
        &topology::json_bytes(&report, 32 * 1024 * 1024)?,
    )?;
    Ok(())
}

fn compare() -> topology::Result<()> {
    use serde_json::{json, Value};
    use std::path::Path;
    let root = Path::new(topology::CAPTURE_ROOT);
    let complete: Value =
        serde_json::from_slice(&topology::read(&root.join("capture-complete.json"), 65536)?)
            .map_err(|_| "complete JSON")?;
    topology::require(
        complete.get("manifest_sha256") == Some(&json!(topology::APPROVED_MANIFEST_SHA))
            && complete.get("preflight_sha256") == Some(&json!(topology::APPROVED_PREFLIGHT_SHA))
            && topology::APPROVED_MANIFEST_SHA.len() == 64
            && topology::APPROVED_PREFLIGHT_SHA.len() == 64,
        "approved comparison inputs",
    )?;
    let captures = complete
        .get("captures")
        .and_then(Value::as_array)
        .ok_or("capture inventory")?;
    topology::require(captures.len() == 9, "capture shards")?;
    let cases = topology::cases()?;
    let mut observations = Vec::new();
    for ((index, shard), entry) in cases.chunks(14).enumerate().zip(captures) {
        topology::require(entry.get("shard") == Some(&json!(index)), "capture index")?;
        let stdout = topology::read(
            &root.join(format!("stdout-{index:02}.raw")),
            8 * 1024 * 1024,
        )?;
        let candidates = topology::read(
            &root.join(format!("candidates-{index:02}.json")),
            8 * 1024 * 1024,
        )?;
        topology::require(
            entry.get("stdout_sha256") == Some(&json!(topology::hash(&stdout)))
                && entry.get("candidate_sha256") == Some(&json!(topology::hash(&candidates))),
            "retained comparison bytes",
        )?;
        let oracle: Value = serde_json::from_slice(&stdout).map_err(|_| "retained oracle JSON")?;
        let candidate: Value =
            serde_json::from_slice(&candidates).map_err(|_| "retained candidate JSON")?;
        let outputs = oracle
            .get("outputs")
            .and_then(Value::as_array)
            .ok_or("outputs")?;
        let candidates = candidate.as_array().ok_or("candidates")?;
        topology::require(
            outputs.len() == shard.len() && candidates.len() == shard.len(),
            "retained exact matrix",
        )?;
        let stderr = topology::read(&root.join(format!("stderr-{index:02}.raw")), 65536)?;
        let status = topology::read(&root.join(format!("capture-{index:02}.status.json")), 4096)?;
        topology::require(
            entry.get("stderr_sha256") == Some(&json!(topology::hash(&stderr)))
                && entry.get("status_sha256") == Some(&json!(topology::hash(&status))),
            "retained operational evidence",
        )?;
        let status: Value = serde_json::from_slice(&status).map_err(|_| "status JSON")?;
        topology::require(
            status.get("success") == Some(&json!(true))
                && status.get("failure").is_some_and(Value::is_null),
            "retained process fault",
        )?;
        qualify_diagnostics(shard, candidates, &stderr)?;
        for ((case, candidate), node) in shard.iter().zip(candidates).zip(outputs) {
            qualify_matrix(case, candidate, node)?;
            observations.push(json!({"id":case.get("id"),"candidate":candidate,"node":node}));
        }
    }
    topology::put(
        &root.join("matrix-report.json"),
        &topology::json_bytes(
            &json!({"observations":observations,"zero_diff":false,"policy_differences":topology::POLICY_DIFFERENCES,"qualification":"finite factory observations only; public routes/gates and resource/dylib ancestry remain held"}),
            32 * 1024 * 1024,
        )?,
    )
}

fn qualify_diagnostics(
    cases: &[serde_json::Value],
    candidates: &[serde_json::Value],
    stderr: &[u8],
) -> topology::Result<()> {
    use std::collections::BTreeMap;
    let mut allowed = BTreeMap::<String, usize>::new();
    for (case, candidate) in cases.iter().zip(candidates) {
        let id = topology::string(case, "id")?;
        let line = if id == "K-missing" {
            Some(format!("Generated credential-encryption key: {}/secrets.key (include this file in backups)",topology::string(candidate,"node_dir")?))
        } else if matches!(
            id,
            "F-secret-crossowner"
                | "F-secret-short"
                | "F-secret-tag"
                | "F-secret-wrongkey"
                | "F-secret-prefix"
        ) {
            Some(format!(
                "storage secret for user {} could not be decrypted; treating it as unset",
                topology::string(case, "owner")?
            ))
        } else {
            None
        };
        if let Some(line) = line {
            *allowed.entry(line).or_default() += 1;
        }
    }
    for line in std::str::from_utf8(stderr)
        .map_err(|_| "diagnostic encoding")?
        .lines()
    {
        let count = allowed
            .get_mut(line)
            .ok_or("unknown operational diagnostic")?;
        topology::require(*count > 0, "repeated unexpected diagnostic")?;
        *count -= 1;
    }
    Ok(())
}

fn expected_refusal(case: &serde_json::Value) -> topology::Result<Option<&'static str>> {
    use serde_json::Value;
    let id = topology::string(case, "id")?;
    if matches!(
        id,
        "K-missing"
            | "K-current31"
            | "K-current33"
            | "K-previous31"
            | "K-previous33"
            | "K-space-env"
            | "K-previous-directory"
    ) {
        return Ok(Some("key refused"));
    }
    let owner = topology::string(case, "owner")?;
    if owner.is_empty() || owner.len() > 65536 {
        return Ok(Some("projection:Owner"));
    }
    let shape = topology::string(case, "shape")?;
    if matches!(
        shape,
        "missing" | "view" | "missing-region" | "no-pk" | "composite-pk"
    ) {
        return Ok(Some("projection:Schema"));
    }
    if case.get("row").is_none_or(Value::is_null) {
        return Ok(None);
    }
    let row = case.get("row").ok_or("row")?;
    if row
        .get("user_id")
        .and_then(|v| v.get("t"))
        .and_then(Value::as_str)
        != Some("text")
    {
        return Ok(None);
    }
    let mut total = 0usize;
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
        let cell = row.get(field).ok_or("row field")?;
        if topology::string(cell, "t")? != "text" {
            return Ok(Some("projection:Row"));
        }
        let s = topology::string(cell, "v")?;
        if s.len() > cap {
            return Ok(Some("projection:Cap"));
        }
        total = total.checked_add(s.len()).ok_or("row total")?;
    }
    if row
        .get("updated_at")
        .and_then(|v| v.get("t"))
        .and_then(Value::as_str)
        != Some("integer")
    {
        return Ok(Some("projection:Row"));
    }
    if total > 262144 || id == "C-opened-65537" {
        return Ok(Some("projection:Cap"));
    }
    Ok(None)
}

fn qualify_matrix(
    case: &serde_json::Value,
    candidate: &serde_json::Value,
    node: &serde_json::Value,
) -> topology::Result<()> {
    use serde_json::{json, Value};
    topology::require(node.get("id") == case.get("id"), "matrix id")?;
    let result = candidate
        .get("candidate")
        .and_then(|v| v.get("result"))
        .ok_or("candidate result")?;
    if let Some(refusal) = expected_refusal(case)? {
        topology::require(
            result.get("refusal").and_then(Value::as_str) == Some(refusal),
            "expected strict candidate refusal",
        )?;
        let node_dir = candidate
            .get("node_dir")
            .and_then(Value::as_str)
            .ok_or("node key directory")?;
        if !topology::qualify_refusal_exception(case, refusal, node, node_dir)? {
            if topology::string(case, "shape")? == "view" {
                topology::assert_view_factory_evidence(case, node)?;
            }
            qualify_sql_delta(case, node)?;
        }
        return Ok(());
    }
    topology::require(
        node.get("exceptionPhase").is_none(),
        "unexpected Node exception",
    )?;
    if topology::string(case, "lane")? == "gate" {
        let components = node.get("components").ok_or("actual auth components")?;
        let raw = node.get("rawCsrf").ok_or("raw CSRF")?;
        let observed = topology::observed_csrf(raw)?;
        topology::require(
            components.get("csrfValid") == Some(&observed),
            "CSRF raw/Boolean observation binding",
        )?;
        topology::require(
            result.get("components") == Some(components),
            "auth component mismatch",
        )?;
        let method = topology::string(case, "method")?;
        let expected = if components.get("owner").is_none_or(Value::is_null) {
            "Unauthorized"
        } else if !matches!(method, "GET" | "HEAD" | "OPTIONS")
            && (components.get("originValid") != Some(&json!(true))
                || components.get("csrfValid") != Some(&json!(true)))
        {
            "Csrf"
        } else if components.get("browserOnly") == Some(&json!(true)) {
            "BrowserOnly"
        } else {
            "accepted"
        };
        topology::require(
            result.get("gate") == Some(&json!(expected)),
            "private actual gate mismatch",
        )?;
        topology::require(
            result.get("projection_count")
                == Some(&json!(if expected == "accepted" { 1 } else { 0 })),
            "private projection after gate",
        )?;
        if expected == "accepted" {
            let p = result.get("projection").ok_or("gated projection")?;
            topology::require(
                p.get("owner") == components.get("owner")
                    && p.get("effectiveRole") == components.get("role"),
                "actual identity binding",
            )?;
            let owner = components
                .get("owner")
                .and_then(Value::as_str)
                .ok_or("owner")?;
            topology::require(
                p.get("secret") == Some(&json!(if owner == "a" { "synthetic-secret" } else { "" })),
                "gated synthetic credential truth",
            )?;
        }
        topology::require(
            node.get("before")
                .and_then(Value::as_array)
                .and_then(|v| v.last())
                == node
                    .get("after")
                    .and_then(Value::as_array)
                    .and_then(|v| v.last()),
            "auth component oracle must not read/reseal storage",
        )?;
        return Ok(());
    }
    // Non-text owner rows cannot be selected by the fixed text owner parameter.
    if topology::string(case, "lane")? == "type"
        && topology::string(case, "id")?.starts_with("T-user_id-")
    {
        let mut missing = case.clone();
        *missing.get_mut("row").ok_or("row")? = Value::Null;
        return qualify_functional(&missing, candidate, node);
    }
    qualify_functional(case, candidate, node)
}

fn qualify_functional(
    case: &serde_json::Value,
    candidate: &serde_json::Value,
    node: &serde_json::Value,
) -> topology::Result<()> {
    use serde_json::{json, Value};
    topology::require(
        node.get("id") == case.get("id") && node.get("owner") == case.get("owner"),
        "case owner binding",
    )?;
    let c = candidate
        .get("candidate")
        .and_then(|v| v.get("result"))
        .and_then(|v| v.get("projection"))
        .ok_or("functional candidate refused")?;
    let n = node.get("storage").ok_or("actual factory result")?;
    let missing = case.get("row").is_none_or(Value::is_null);
    if missing {
        topology::assert_missing_factory_shape(n)?;
    }
    let original_kind = if missing {
        Some("local")
    } else {
        case.get("row")
            .and_then(|r| r.get("kind"))
            .and_then(|v| v.get("v"))
            .and_then(Value::as_str)
    };
    topology::require(
        n.get("kind").and_then(Value::as_str) == original_kind,
        "Node raw kind truth",
    )?;
    let kind = match n.get("kind").and_then(Value::as_str) {
        Some("webdav") => "WebDav",
        Some("nextcloud") => "Nextcloud",
        Some("s3") => "S3",
        _ => "Local",
    };
    let mut expected = n.clone();
    let object = expected.as_object_mut().ok_or("factory object")?;
    object.insert("owner".into(), case.get("owner").cloned().ok_or("owner")?);
    object.insert("effectiveRole".into(), json!("member"));
    object.insert("kind".into(), json!(kind));
    if missing {
        object.insert("secret".into(), json!(""));
        object.insert("secretConfigured".into(), json!(false));
        object.insert("secretNeedsReauth".into(), json!(false));
    }
    topology::compare_private_projection(&expected, c)?;
    qualify_sql_delta(case, node)?;
    let plain = n.get("secret").and_then(Value::as_str).unwrap_or("");
    if !missing && !plain.is_empty() {
        topology::require(
            node.get("storedPlain").and_then(Value::as_str) == Some(plain),
            "stored credential truth",
        )?;
    }
    if case.get("public").and_then(Value::as_bool) == Some(true) {
        let metadata = node.get("metadata").ok_or("metadata")?;
        let false_storage = node.get("falseStorage").ok_or("fresh false storage")?;
        topology::require(
            false_storage.get("secret").is_none(),
            "fresh includeSecret=false leaked secret",
        )?;
        let mut public = n.clone();
        public
            .as_object_mut()
            .ok_or("public object")?
            .remove("secret");
        topology::require(false_storage == &public, "fresh false factory body")?;
        let mut false_node = node.clone();
        *false_node.get_mut("before").ok_or("false before slot")? =
            node.get("falseBefore").cloned().ok_or("false before")?;
        *false_node.get_mut("after").ok_or("false after slot")? =
            node.get("falseAfter").cloned().ok_or("false after")?;
        for (target, source) in [
            ("stored", "falseStored"),
            ("storedPlain", "falseStoredPlain"),
            ("currentOpen", "falseCurrentOpen"),
            ("foreignOpen", "falseForeignOpen"),
            ("storedEvidence", "falseStoredEvidence"),
        ] {
            false_node.as_object_mut().ok_or("false object")?.insert(
                target.into(),
                node.get(source).cloned().ok_or("false key evidence")?,
            );
        }
        qualify_sql_delta(case, &false_node)?;
        topology::require(
            metadata.get("status") == Some(&json!(200)),
            "actual metadata status",
        )?;
        topology::require(
            metadata.get("body").and_then(|v| v.get("secret")).is_none(),
            "public secret leaked",
        )?;
        topology::require(
            metadata.get("headers")
                == Some(&json!({"Content-Type":"application/json","Cache-Control":"no-store"})),
            "actual factory HTTP headers",
        )?;
        topology::require(
            metadata.get("body") == Some(false_storage),
            "actual public metadata body",
        )?;
    }
    Ok(())
}

fn qualify_sql_structure(
    case: &serde_json::Value,
    node: &serde_json::Value,
) -> topology::Result<bool> {
    use serde_json::Value;
    let before = node
        .get("before")
        .and_then(Value::as_array)
        .ok_or("before state")?;
    let after = node
        .get("after")
        .and_then(Value::as_array)
        .ok_or("after state")?;
    topology::require(
        before.len() == topology::STATE_SQL.len() && after.len() == before.len(),
        "whole state inventory",
    )?;
    for (i, (a, b)) in before.iter().zip(after).enumerate() {
        if i + 1 != topology::STATE_SQL.len() {
            topology::require(a == b, "unexpected non-storage SQL delta")?;
        }
    }
    let old = before.last().ok_or("storage before")?;
    let new = after.last().ok_or("storage after")?;
    if topology::string(case, "shape")? == "view" {
        topology::require(old == new, "view SQL changed")?;
        return Ok(false);
    }
    topology::require(
        old.get("columns") == new.get("columns"),
        "storage schema changed",
    )?;
    let columns = old
        .get("columns")
        .and_then(Value::as_array)
        .ok_or("storage columns")?;
    let secret_column = columns
        .iter()
        .position(|v| v.as_str() == Some("secret"))
        .ok_or("secret column")?;
    let a = old
        .get("rows")
        .and_then(Value::as_array)
        .ok_or("storage rows")?;
    let b = new
        .get("rows")
        .and_then(Value::as_array)
        .ok_or("storage rows")?;
    topology::require(a.len() == b.len(), "storage row count changed")?;
    let owner_column = columns
        .iter()
        .position(|v| v.as_str() == Some("user_id"))
        .ok_or("owner column")?;
    let mut changed = false;
    let mut foreign_seen = false;
    for (a, b) in a.iter().zip(b) {
        let a = a.as_array().ok_or("typed row")?;
        let b = b.as_array().ok_or("typed row")?;
        topology::require(
            a.len() == columns.len() && b.len() == a.len(),
            "typed row length",
        )?;
        let selected = a.get(owner_column).and_then(|v| v.get("v")) == case.get("owner")
            && a.get(owner_column)
                .and_then(|v| v.get("t"))
                .and_then(Value::as_str)
                == Some("text");
        if a.get(owner_column)
            .and_then(|v| v.get("v"))
            .and_then(Value::as_str)
            == Some("foreign")
        {
            foreign_seen = true;
        }
        for (i, (a, b)) in a.iter().zip(b).enumerate() {
            if i != secret_column || !selected {
                topology::require(a == b, "unapproved storage cell changed")?;
            } else if a != b {
                changed = true;
            }
        }
    }
    if topology::foreign_case(case)?.is_some() {
        topology::require(foreign_seen, "foreign credential row missing")?;
    }
    Ok(changed)
}

fn qualify_sql_delta(case: &serde_json::Value, node: &serde_json::Value) -> topology::Result<()> {
    use serde_json::{json, Value};
    let changed = qualify_sql_structure(case, node)?;
    let missing = case.get("row").is_none_or(Value::is_null);
    let plain = node.get("storedPlain").and_then(Value::as_str);
    if changed {
        topology::require(!missing, "missing selected row changed")?;
        let current = node.get("currentOpen").ok_or("current key evidence")?;
        topology::require(
            current.get("keyUsed") == Some(&json!("current"))
                && current.get("plain").and_then(Value::as_str) == plain
                && plain.is_some_and(|s| !s.is_empty()),
            "changed credential current-key truth",
        )?;
        if topology::string(case, "owner")?.is_empty() {
            topology::require(
                node.get("stored")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.starts_with("enc:v1:"))
                    && node
                        .get("foreignOpen")
                        .and_then(|v| v.get("plain"))
                        .and_then(Value::as_str)
                        == plain,
                "explicit empty-owner unbound Node difference",
            )?;
        } else {
            topology::assert_js_gcm_auth_failure(
                node.get("foreignOpen").ok_or("foreign open evidence")?,
            )?;
            topology::require(
                node.get("stored")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.starts_with("enc:v2:")),
                "changed credential v2",
            )?;
        }
    }
    if !missing && plain.is_some_and(|s| !s.is_empty()) {
        let original = case
            .get("row")
            .and_then(|r| r.get("secret"))
            .and_then(|s| s.get("v"))
            .ok_or("original synthetic secret")?;
        if !original.as_str().is_some_and(|s| s.starts_with("enc:v2:")) {
            topology::require(
                node.get("stored").and_then(Value::as_str).is_some_and(|s| {
                    s.starts_with(
                        if topology::string(case, "owner").is_ok_and(str::is_empty) {
                            "enc:v1:"
                        } else {
                            "enc:v2:"
                        },
                    ) && Some(s) != original.as_str()
                }),
                "actual Node lazy reseal missing",
            )?;
        } else {
            topology::require(
                node.get("stored") == Some(original),
                "v2 unexpectedly changed",
            )?;
        }
    } else {
        topology::require(!changed, "unreadable/empty/missing changed SQL")?;
    }
    Ok(())
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match topology::stage(&args) {
        Some(topology::Stage::Materialize) => topology::materialize(FACTORY_ADAPTER).map(|_| ()),
        Some(topology::Stage::Preflight) => preflight(),
        Some(topology::Stage::Capture) => run(),
        Some(topology::Stage::Compare) => compare(),
        None => {
            eprintln!("one exact separately approved stage required");
            std::process::exit(2);
        }
    };
    if let Err(error) = result {
        eprintln!("private factory qualification refused: {error}");
        std::process::exit(1);
    }
}
