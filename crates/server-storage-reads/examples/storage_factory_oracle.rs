//! Stage A source artifact. No production integration or runtime qualification.
#[path = "../../../tools/qualification/process.rs"]
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
(async()=>{const outputs=[];for(const c of input.cases){let auth;let phase='keys';try {
 const secrets = createSecretStore(c.dataDir,{env:c.env});
 phase='boot';auth = createAuth({dataDir:c.dataDir,publicOrigin:'https://owner.invalid',rpId:'owner.invalid',secrets,trustProxy:true});
 const snapshot=()=>input.stateSQL.map(sql=>{try{const q=auth.db.prepare(sql);return {columns:q.columns().map(v=>v.name),rows:q.all().map(row=>q.columns().map(({name})=>{const v=row[name];return v===null?{t:'null',v:null}:Buffer.isBuffer(v)?{t:'blob',v:v.toString('hex')}:typeof v==='string'?{t:'text',v}:Number.isInteger(v)?{t:'integer',v}:{t:'real',v};}))};}catch{return {unavailable:true};}});
 const bootAfter=snapshot();
 if(c.shapeSQL)auth.db.exec(c.shapeSQL);
 if(c.insertSQL)auth.db.prepare(c.insertSQL).run(c.cells.map(v=>v.t==='blob'?Buffer.from(v.v,'hex'):v.v));
 const before=snapshot();
 phase='factory';const device = createDeviceAuth({db:auth.db,audit:auth.audit,publicUser:auth.publicUser,origin:()=>auth.origin,now:()=>input.now,addressesTrusted:true});
 const requestAuth = createRequestAuth({enabled:()=>c.deviceEnabled,deviceAuth:device,authService:auth});
 const req={method:c.method,headers:c.headers};
 const authn=requestAuth.authenticate(req);
 const components={owner:authn?.user.id??null,role:authn?.user.role??null,device:authn?!!authn.device:null,originValid:auth.originValid(req),csrfValid:authn?requestAuth.csrfValid(req,authn):null,browserOnly:authn?requestAuth.browserOnly(authn,c.path,c.method):null};
 const factoryOwner=c.gate?null:c.owner;
 const storage=factoryOwner===null?null:auth.getStorage(factoryOwner,true);
 let metadata=null;
 if(c.public){
  if(!authn)throw new Error('synthetic authenticated metadata fixture refused');
  const res={writeHead(status,headers){this.status=status;this.headers=headers;},end(body){metadata={status:this.status,headers:this.headers,body:JSON.parse(body)};}};
  const routes=createStorageRoutes({json:http.json,authService:auth});
  await routes.handle({method:'GET'},res,{path:'/api/integrations/storage',authn});
 }
 const after=snapshot();
 const stored=auth.db.prepare('SELECT secret FROM storage_connections WHERE user_id=?').get(factoryOwner)?.secret;
 let storedPlain=null;try{if(stored!==undefined)storedPlain=secrets.decrypt(stored,factoryOwner);}catch{storedPlain=null;}
 outputs.push({id:c.id,owner:factoryOwner,components,storage,metadata,bootAfter,before,after,stored,storedPlain});
}catch{outputs.push({id:c.id,exceptionPhase:phase});}finally{if(auth)auth.db.close();}}
 const modules=Object.keys(require.cache).map(p=>fs.realpathSync(p)).sort();
 process.stdout.write(JSON.stringify({runtime:{version:process.version,abi:process.versions.modules,platform:process.platform,arch:process.arch},outputs,modules}));
})().catch(()=>{process.exitCode=1;});
"#;

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
    let manifest = topology::materialize(FACTORY_ADAPTER)?;
    let root = Path::new(topology::ROOT);
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
    let mut observations = Vec::new();
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
            inputs.push(json!({"id":case.get("id"),"dataDir":node_dir,"owner":case.get("owner"),"gate":topology::string(case,"lane")?=="gate","env":{"SECRETS_KEY_PREVIOUS":case.get("previous_env").and_then(Value::as_str).unwrap_or("")},"deviceEnabled":case.get("device_enabled"),"headers":case.get("headers"),"method":case.get("method"),"path":case.get("path"),"public":case.get("public"),"shapeSQL":topology::shape_sql(topology::string(case,"shape")?)?,"insertSQL":topology::insert_sql(case)?,"cells":cells}));
            candidates
                .push(json!({"id":case.get("id"),"candidate":candidate,"boot_before":boot_before}));
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
        cmd.current_dir(root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .args(["--no-warnings", "qualification-adapter.cjs"]);
        topology::put(
            &root.join(format!("command-{shard_index:02}.json")),
            &topology::json_bytes(
                &json!({"program":topology::NODE,"args":["--no-warnings","qualification-adapter.cjs"],"cwd":root,"env":{"PATH":"/usr/bin:/bin"},"input_sha256":topology::hash(&input),"node_sha256":topology::NODE_SHA,"adapter_sha256":topology::hash(FACTORY_ADAPTER.as_bytes())}),
                65536,
            )?,
        )?;
        topology::verify_materialized(&manifest)?;
        let output = match process::bounded(cmd, input, Duration::from_secs(30)) {
            Ok(output) => output,
            Err(error) => {
                topology::put(
                    &root.join(format!("process-error-{shard_index:02}.txt")),
                    error.as_bytes(),
                )?;
                return Err("bounded process failed; no retry".into());
            }
        };
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
                &json!({"code":output.status.code(),"success":output.status.success()}),
                1024,
            )?,
        )?;
        topology::require(
            output.status.success(),
            "actual Node process failed; no retry",
        )?;
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
        for ((case, candidate), node) in shard.iter().zip(candidates).zip(outputs) {
            qualify_matrix(case, &candidate, node)?;
            observations.push(json!({"id":case.get("id"),"candidate":candidate,"node":node,"classification":"functional comparison plus explicit readonly lazy-reseal difference"}));
        }
    }
    let report = json!({"actual_core":topology::CORE,"candidate_production":topology::CANDIDATE,"source_cases":126,"policy_differences":topology::POLICY_DIFFERENCES,"observations":observations,"qualification":"finite fixed matrix only; gate lane compares actual components, private candidate gate and projector count, not Node index HTTP routing","zero_diff":false,"resource_qualification":"logical caps/cooperative/trusted stable topology; no RSS/OOM/kernel/Linux/escaped-descendant/FD-ancestry claim"});
    topology::put(
        &root.join("matrix-report.json"),
        &topology::json_bytes(&report, 32 * 1024 * 1024)?,
    )?;
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
        let phase = node.get("exceptionPhase").and_then(Value::as_str);
        if refusal == "key refused" && topology::string(case, "id")? != "K-missing" {
            topology::require(phase == Some("keys"), "actual key infrastructure refusal")?;
        } else {
            topology::require(
                phase.is_none() || phase == Some("factory"),
                "unexpected Node infrastructure failure",
            )?;
        }
        return Ok(());
    }
    topology::require(
        node.get("exceptionPhase").is_none(),
        "unexpected Node exception",
    )?;
    if topology::string(case, "lane")? == "gate" {
        let components = node.get("components").ok_or("actual auth components")?;
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
    for (a, b) in a.iter().zip(b) {
        let a = a.as_array().ok_or("typed row")?;
        let b = b.as_array().ok_or("typed row")?;
        topology::require(
            a.len() == columns.len() && b.len() == a.len(),
            "typed row length",
        )?;
        for (i, (a, b)) in a.iter().zip(b).enumerate() {
            if i != secret_column {
                topology::require(a == b, "noncredential storage cell changed")?;
            }
        }
    }
    let plain = n.get("secret").and_then(Value::as_str).unwrap_or("");
    if !missing && !plain.is_empty() {
        topology::require(
            node.get("storedPlain").and_then(Value::as_str) == Some(plain),
            "stored credential truth",
        )?;
        let original = case
            .get("row")
            .and_then(|r| r.get("secret"))
            .and_then(|s| s.get("v"))
            .and_then(Value::as_str)
            .ok_or("original synthetic secret")?;
        if !original.starts_with("enc:v2:") {
            topology::require(
                node.get("stored")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.starts_with("enc:v2:") && s != original),
                "actual Node lazy reseal missing",
            )?;
        } else {
            topology::require(
                node.get("stored").and_then(Value::as_str) == Some(original),
                "v2 unexpectedly changed",
            )?;
        }
    } else {
        topology::require(
            before.last() == after.last(),
            "unreadable/empty/missing changed SQL",
        )?;
    }
    if case.get("public").and_then(Value::as_bool) == Some(true) {
        let metadata = node.get("metadata").ok_or("metadata")?;
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
        let mut public = n.clone();
        public
            .as_object_mut()
            .ok_or("public object")?
            .remove("secret");
        topology::require(
            metadata.get("body") == Some(&public),
            "actual public metadata body",
        )?;
    }
    Ok(())
}

fn main() {
    if std::env::args().skip(1).collect::<Vec<_>>()
        != ["--approved-fixed-stage-b"].map(String::from)
    {
        eprintln!("exact future Stage B invocation required; Stage A does not execute");
        std::process::exit(2);
    }
    if let Err(error) = run() {
        eprintln!("private factory qualification refused: {error}");
        std::process::exit(1);
    }
}
