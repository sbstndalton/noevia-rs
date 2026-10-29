//! Synthetic qualification only: run actual pinned Node corpus code with an existing input seam.
use std::os::unix::process::CommandExt;
use std::{
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{SystemTime, UNIX_EPOCH},
};
const MEMINFO: &str = "MemTotal:        8388608 kB\n";
#[path = "support/prompts_dependencies.rs"]
mod dependencies;
#[allow(dead_code)]
#[path = "../../../qualification/process.rs"]
mod process;
fn bounded(command: Command) -> Result<process::Output, String> {
    process::bounded(command, Vec::new(), std::time::Duration::from_secs(600))
}
fn capture_domain() -> (&'static str, u64) {
    if std::env::var("NOEVIA_ROUTING_CORPUS").as_deref() == Ok("1") {
        ("routing-mode", 255)
    } else {
        ("baseline", 228)
    }
}
fn validate_capture(
    manifest: &serde_json::Value,
    domain: &str,
    count: u64,
    recorded: u64,
) -> Result<(), String> {
    if manifest
        .get("exchanges")
        .and_then(serde_json::Value::as_u64)
        != Some(count)
        || manifest.get("dropped").and_then(serde_json::Value::as_u64) != Some(0)
        || recorded != count
        || manifest
            .get("captureDomain")
            .is_some_and(|value| value.as_str() != Some(domain))
    {
        return Err("actual corpus capture domain/count/drop mismatch".into());
    }
    Ok(())
}
fn finalize_capture(core: &Path, out: &Path) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    let (domain, count) = capture_domain();
    let path = out.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let recorded = std::fs::read_dir(out.join("exchanges"))
        .map_err(|e| e.to_string())?
        .try_fold(0_u64, |count, entry| -> Result<u64, String> {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.file_type().map_err(|e| e.to_string())?.is_file()
                || entry.path().extension().and_then(|e| e.to_str()) != Some("json")
            {
                return Err("unexpected capture entry".into());
            }
            Ok(count + 1)
        })?;
    validate_capture(&manifest, domain, count, recorded)?;
    let owned: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../contracts/http/corpus/manifest.json"
    ))
    .map_err(|e| e.to_string())?;
    let calendar = owned
        .get("calendarContract")
        .ok_or("owned calendar contract missing")?
        .clone();
    if calendar != serde_json::json!({"version":1,"timeZone":"UTC","usageWindowDays":365}) {
        return Err("owned calendar contract changed".into());
    }
    let object = manifest
        .as_object_mut()
        .ok_or("capture manifest not object")?;
    object.insert("calendarContract".into(), calendar);
    object.insert("captureDomain".into(), domain.into());
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let hash = |file: &Path| -> Result<String, String> {
        Ok(format!(
            "{:x}",
            Sha256::digest(std::fs::read(file).map_err(|e| e.to_string())?)
        ))
    };
    let wasm = hash(&core.join("server/wasm/dav-parse.wasm"))?;
    if wasm != "c5c300907dba8ed36b38bbfcd92dc33f18be77319712504106abb09d5b8ce05b" {
        return Err("actual core WASM provenance mismatch".into());
    }
    let mut command = Command::new("node");
    command.arg("--version");
    let version = bounded(command)?;
    let node = String::from_utf8_lossy(&version.stdout).trim().to_owned();
    if !version.status.success() || node != "v22.23.3" {
        return Err("actual capture requires Node22.23.3".into());
    }
    let provenance = serde_json::json!({"core":include_str!("../../../../contracts/http/core.ref").trim(),"coreArchiveSha256":include_str!("../../../../contracts/http/core.sha256").trim(),"captureDomain":domain,"node":node,"generatorSha256":hash(&core.join("tools/contract-corpus/generate.cjs"))?,"wasmSha256":wasm,"generator":"actual core tools/contract-corpus/generate.cjs","hostInput":{"module":"server/inference-budget.cjs","collaborator":"readMeminfo","memTotalKib":8388608},"exchanges":count,"dropped":0});
    std::fs::write(
        out.join("SOURCE_ORACLE.json"),
        serde_json::to_vec_pretty(&provenance).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    println!("actual capture domain={domain} count={count} zero drops verified");
    Ok(())
}
fn js(value: &str) -> Result<String, String> {
    serde_json::to_string(value).map_err(|e| e.to_string())
}
fn temporary() -> Result<PathBuf, String> {
    let tick = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    for attempt in 0..32 {
        let p = std::env::temp_dir().join(format!(
            "noevia-synthetic-host-{}-{tick}-{attempt}",
            std::process::id()
        ));
        match std::fs::create_dir(&p) {
            Ok(()) => return Ok(p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    Err("cannot allocate synthetic fixture directory".into())
}
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mode = args
        .next()
        .ok_or("usage: corpus-launcher capture|serve|front-serve|verify|wrong-host CORE [OUT]")?;
    let core = PathBuf::from(args.next().ok_or("actual core checkout required")?)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let output = args.next();
    if args.next().is_some() {
        return Err("too many arguments".into());
    }
    let mut command = Command::new("git");
    command.arg("-C").arg(&core).args(["rev-parse", "HEAD"]);
    let sha = bounded(command)?;
    // The front's upstream is the exact Core generation paired with it (front-core.ref): the
    // release guard refuses mixed feature owners. core.ref stays the recording oracle.
    let pinned = if mode == "front-serve" {
        include_str!("../../../../contracts/http/front-core.ref")
    } else {
        include_str!("../../../../contracts/http/core.ref")
    };
    if !sha.status.success() || String::from_utf8_lossy(&sha.stdout).trim() != pinned.trim() {
        return Err("synthetic launcher requires the actual pinned core commit".into());
    }
    if mode == "finalize-capture" {
        return finalize_capture(
            &core,
            Path::new(&output.ok_or("capture artifact directory required")?),
        );
    }
    let prompts_port = if mode.starts_with("prompts-") {
        Some(prompt_port(
            output.as_deref().ok_or("synthetic prompt port required")?,
        )?)
    } else {
        None
    };
    let (entry, argv) = match mode.as_str() {
        "capture" => (
            "generate.cjs",
            vec![
                "--out".to_string(),
                output.ok_or("capture requires output directory")?,
            ],
        ),
        "prompts-serve" | "prompts-absent" | "prompts-wrong-token" | "prompts-port-omitted"
            if output.is_some() =>
        {
            ("serve.cjs", vec![])
        }
        "serve" | "front-serve" | "verify" | "verify-omission" | "wrong-host"
            if output.is_none() =>
        {
            ("serve.cjs", vec![])
        }
        _ => return Err("unsupported synthetic launcher arguments".into()),
    };
    let directory = temporary()?;
    let result = prepare(
        &core,
        &directory,
        entry,
        &argv,
        prompts_port,
        &mode,
        if mode == "wrong-host" {
            "MemTotal:        12582912 kB\n"
        } else {
            MEMINFO
        },
    );
    let wrapper = match result {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&directory);
            return Err(e);
        }
    };
    if mode == "verify" || mode == "verify-omission" {
        if mode == "verify-omission" {
            let source = std::fs::read_to_string(&wrapper).map_err(|e| e.to_string())?;
            let omitted = source.replace("[args[0],args[1],'--require',preload,args[2]]", "args");
            if omitted == source {
                return Err("fixture omission control target missing".into());
            }
            std::fs::write(&wrapper, omitted).map_err(|e| e.to_string())?;
        }
        let verify = include_str!("../synthetic-host-verify.cjs")
            .replace("__WRAPPER__", &js(&wrapper.to_string_lossy())?)
            .replace("__CORE__", &js(&core.to_string_lossy())?);
        let mut command = Command::new("node");
        command.arg("-e").arg(verify);
        let status = bounded(command);
        let _ = std::fs::remove_dir_all(&directory);
        return match status {
            Ok(s) if s.status.success() && mode == "verify" => {
                print!("{}", String::from_utf8_lossy(&s.stdout));
                Ok(())
            }
            Ok(s)
                if !s.status.success()
                    && mode == "verify-omission"
                    && String::from_utf8_lossy(&s.stderr)
                        .contains("FIXTURE_SPAWN_PRELOAD_REQUIRED") =>
            {
                println!("missing fixture seam rejected by actual spawn qualification");
                Ok(())
            }
            Ok(_) => Err("actual spawn/factory qualification failed".into()),
            Err(e) => Err(e),
        };
    }
    if mode == "capture" {
        let mut command = Command::new("node");
        command.arg(&wrapper);
        let result = bounded(command);
        let _ = std::fs::remove_dir_all(&directory);
        let result = result?;
        print!("{}", String::from_utf8_lossy(&result.stdout));
        eprint!("{}", String::from_utf8_lossy(&result.stderr));
        if !result.status.success() {
            return Err("actual Node capture failed".into());
        }
        return finalize_capture(
            &core,
            Path::new(argv.get(1).ok_or("capture output missing")?),
        );
    }
    // Replacing this process preserves the actual Node launcher's signal and child-reap handlers.
    let error = Command::new("node").arg(&wrapper).exec();
    let _ = std::fs::remove_dir_all(&directory);
    Err(error.to_string())
}
fn prompt_port(raw: &str) -> Result<u16, String> {
    if raw.is_empty() || raw.len() > 5 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err("invalid synthetic prompt port".into());
    }
    raw.parse::<u16>()
        .ok()
        .filter(|p| *p != 0)
        .ok_or_else(|| "invalid synthetic prompt port".into())
}
fn prompts_override(port: Option<u16>, control: &str) -> Result<String, String> {
    let Some(port) = port else {
        return Ok(String::new());
    };
    if control == "prompts-absent" {
        return Ok(String::new());
    }
    let url = js(&format!("http://127.0.0.1:{port}"))?;
    let token = js(if control == "prompts-wrong-token" {
        "synthetic-prompts-wrong-token-0123456789abcdef"
    } else {
        "synthetic-prompts-publicfront-token-0123456789abcdef"
    })?;
    let ports = if control == "prompts-port-omitted" {
        String::new()
    } else {
        format!(",NOEVIA_CORPUS_ALLOWED_PORTS:options.env.NOEVIA_CORPUS_ALLOWED_PORTS+',{port}'")
    };
    Ok(format!("options={{...options,env:{{...options.env,MODEL_LOADER_URL:{url},MODEL_LOADER_TOKEN:{token}{ports}}}}};"))
}
fn prepare(
    core: &Path,
    directory: &Path,
    entry: &str,
    argv: &[String],
    prompts_port: Option<u16>,
    prompts_control: &str,
    meminfo: &str,
) -> Result<PathBuf, String> {
    let dependencies_override = if prompts_port.is_some() {
        let revision = include_str!("../../../../contracts/http/core.ref").trim();
        let mut expected = [Vec::new(), Vec::new()];
        for (index, name) in ["package.json", "package-lock.json"].iter().enumerate() {
            let mut command = Command::new("git");
            command
                .env_clear()
                .env("PATH", std::env::var_os("PATH").ok_or("PATH required")?)
                .arg("-C")
                .arg(core)
                .args(["show", &format!("{revision}:server/{name}")]);
            let output = process::bounded(command, Vec::new(), std::time::Duration::from_secs(5))?;
            if !output.status.success() {
                return Err("dependency immutable manifest unavailable".into());
            }
            *expected.get_mut(index).ok_or("dependency manifest index")? = output.stdout;
        }
        let parameter = std::env::var_os("NOEVIA_PROMPTS_DEPENDENCIES").map(PathBuf::from);
        let (modules, _) = dependencies::verify(core, core, parameter.as_deref(), &expected)?;
        dependency_override(&modules)?
    } else {
        String::new()
    };
    let preload = directory.join("host-input.cjs");
    let fixture = include_str!("../synthetic-host-input.cjs")
        .replace(
            "__BUDGET__",
            &js(&core.join("server/inference-budget.cjs").to_string_lossy())?,
        )
        .replace("__MEMINFO__", &js(meminfo)?);
    std::fs::write(&preload, fixture).map_err(|e| e.to_string())?;
    let wrapper = directory.join("launch.cjs");
    let source = include_str!("../synthetic-host-launch.cjs")
        .replace(
            "__SERVER__",
            &js(&core.join("server/index.cjs").to_string_lossy())?,
        )
        .replace(
            "__GUARD__",
            &js(&core
                .join("tools/contract-corpus/net-guard.cjs")
                .to_string_lossy())?,
        )
        .replace("__PRELOAD__", &js(&preload.to_string_lossy())?)
        .replace(
            "__ENTRY__",
            &js(&core
                .join("tools/contract-corpus")
                .join(entry)
                .to_string_lossy())?,
        )
        .replace(
            "__ARGS__",
            &serde_json::to_string(argv).map_err(|e| e.to_string())?,
        )
        .replace("__TEMP__", &js(&directory.to_string_lossy())?)
        .replace(
            "__PROMPTS_OVERRIDE__",
            &format!(
                "{}{dependencies_override}",
                prompts_override(prompts_port, prompts_control)?
            ),
        );
    std::fs::write(&wrapper, source).map_err(|e| e.to_string())?;
    Ok(wrapper)
}
fn dependency_override(modules: &Path) -> Result<String, String> {
    Ok(format!(
        "options={{...options,env:{{...options.env,NODE_PATH:{}}}}};",
        js(modules.to_str().ok_or("dependency path UTF8")?)?
    ))
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("corpus-launcher: {e}");
            ExitCode::from(2)
        }
    }
}
#[cfg(test)]
mod capture_tests {
    use super::*;
    #[test]
    #[allow(clippy::unwrap_used)]
    fn verified_dependency_override_preserves_literal_spawn_options() {
        let directory = tempfile::tempdir().unwrap();
        let fixture = directory.path().join("entry.cjs");
        let wrapper = directory.path().join("launch.cjs");
        std::fs::write(&fixture,"require('node:child_process').spawn(process.execPath,['--require','synthetic-guard','synthetic-server'],{cwd:'synthetic-cwd',stdio:['ignore','pipe','pipe'],env:{PATH:'synthetic-path',NODE_ENV:'production',NOEVIA_CORPUS_ALLOWED_PORTS:'333'}});").unwrap();
        let modules = directory.path().join("external/node_modules");
        let source = include_str!("../synthetic-host-launch.cjs")
            .replace("__SERVER__", "'synthetic-server'")
            .replace("__GUARD__", "'synthetic-guard'")
            .replace("__PRELOAD__", "'synthetic-preload'")
            .replace("__ENTRY__", &js(&fixture.to_string_lossy()).unwrap())
            .replace("__ARGS__", "[]")
            .replace(
                "__TEMP__",
                &js(&directory.path().to_string_lossy()).unwrap(),
            )
            .replace(
                "__PROMPTS_OVERRIDE__",
                &dependency_override(&modules).unwrap(),
            );
        std::fs::write(&wrapper, source).unwrap();
        let script="const assert=require('node:assert/strict');assert.ok(process.version.startsWith('v22.'));const expected=process.argv[2];require('node:child_process').spawn=(command,args,options)=>{assert.equal(command,process.execPath);assert.deepEqual(args,['--require','synthetic-guard','--require','synthetic-preload','synthetic-server']);assert.equal(options.cwd,'synthetic-cwd');assert.deepEqual(options.stdio,['ignore','pipe','pipe']);assert.deepEqual(options.env,{PATH:'synthetic-path',NODE_ENV:'production',NOEVIA_CORPUS_ALLOWED_PORTS:'333',NODE_PATH:expected});console.log('literal spawn retains args/options/netguard and one explicit dependency path');return {};};require(process.argv[1]);";
        // rs#268: a cold runner spent the whole 5 s budget just loading the node binary. Start the
        // interpreter once first, under the same generous bound the other launches in this file
        // use, so that one-time cost (page cache, dynamic linking) is paid outside the measured
        // run. The run below keeps its 5 s bound on the script itself: a hang still fails it.
        let node = || {
            let mut command = Command::new("node");
            command
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap());
            command
        };
        let mut warm = node();
        warm.args(["-e", "process.exit(0)"]);
        let warmed = bounded(warm).unwrap();
        assert!(
            warmed.status.success(),
            "{}",
            String::from_utf8_lossy(&warmed.stderr)
        );
        let mut command = node();
        command.args(["-e", script]).arg(&wrapper).arg(&modules);
        let output =
            process::bounded(command, Vec::new(), std::time::Duration::from_secs(5)).unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "literal spawn retains args/options/netguard and one explicit dependency path\n"
        );
    }
    #[test]
    fn prompt_override_is_explicit_bounded_and_default_absent() -> Result<(), String> {
        assert_eq!(prompts_override(None, "serve")?, "");
        assert_eq!(prompt_port("8090")?, 8090);
        for port in ["0", "65536", "-1", " 8090", "8090/path", "localhost", "1\n"] {
            assert!(prompt_port(port).is_err());
        }
        let override_ = prompts_override(Some(8090), "prompts-serve")?;
        assert!(override_.contains("MODEL_LOADER_URL:\"http://127.0.0.1:8090\""));
        assert!(override_.contains(
            "NOEVIA_CORPUS_ALLOWED_PORTS:options.env.NOEVIA_CORPUS_ALLOWED_PORTS+',8090'"
        ));
        assert_eq!(prompts_override(Some(8090), "prompts-absent")?, "");
        assert!(prompts_override(Some(8090), "prompts-wrong-token")?
            .contains("synthetic-prompts-wrong-token"));
        let omitted = prompts_override(Some(8090), "prompts-port-omitted")?;
        assert!(omitted.contains("MODEL_LOADER_URL"));
        assert!(!omitted.contains("NOEVIA_CORPUS_ALLOWED_PORTS"));
        Ok(())
    }
    #[test]
    fn exact_domains_counts_and_zero_drops() {
        let baseline = serde_json::json!({"exchanges":228,"dropped":0,"captureDomain":"baseline"});
        let routing =
            serde_json::json!({"exchanges":255,"dropped":0,"captureDomain":"routing-mode"});
        assert!(validate_capture(&baseline, "baseline", 228, 228).is_ok());
        assert!(validate_capture(&routing, "routing-mode", 255, 255).is_ok());
        assert!(validate_capture(&baseline, "routing-mode", 255, 228).is_err());
        assert!(validate_capture(&routing, "baseline", 228, 255).is_err());
        assert!(validate_capture(&routing, "other", 255, 255).is_err());
        assert!(validate_capture(&routing, "routing-mode", 255, 254).is_err());
        assert!(validate_capture(
            &serde_json::json!({"exchanges":255,"dropped":1}),
            "routing-mode",
            255,
            255
        )
        .is_err());
    }
}
