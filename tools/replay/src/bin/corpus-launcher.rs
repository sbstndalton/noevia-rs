//! Synthetic qualification only: run actual pinned Node corpus code with an existing input seam.
use std::os::unix::process::CommandExt;
use std::{
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{SystemTime, UNIX_EPOCH},
};
const MEMINFO: &str = "MemTotal:        8388608 kB\n";
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
        .ok_or("usage: corpus-launcher capture|serve|verify|wrong-host CORE [OUT]")?;
    let core = PathBuf::from(args.next().ok_or("actual core checkout required")?)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let output = args.next();
    if args.next().is_some() {
        return Err("too many arguments".into());
    }
    let sha = Command::new("git")
        .arg("-C")
        .arg(&core)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| e.to_string())?;
    if !sha.status.success()
        || String::from_utf8_lossy(&sha.stdout).trim()
            != include_str!("../../../../contracts/http/core.ref").trim()
    {
        return Err("synthetic launcher requires the actual pinned core commit".into());
    }
    let (entry, argv) = match mode.as_str() {
        "capture" => (
            "generate.cjs",
            vec![
                "--out".to_string(),
                output.ok_or("capture requires output directory")?,
            ],
        ),
        "serve" | "verify" | "verify-omission" | "wrong-host" if output.is_none() => {
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
        let status = Command::new("node")
            .arg("-e")
            .arg(verify)
            .output()
            .map_err(|e| e.to_string());
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
        let status = Command::new("node")
            .arg(&wrapper)
            .status()
            .map_err(|e| e.to_string());
        let _ = std::fs::remove_dir_all(&directory);
        match status {
            Ok(status) if status.success() => {}
            Ok(_) => return Err("actual Node capture failed".into()),
            Err(e) => return Err(e),
        }
        let out = argv.get(1).ok_or("capture output missing")?;
        let manifest_path = Path::new(out).join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        if manifest
            .get("exchanges")
            .and_then(serde_json::Value::as_u64)
            != Some(193)
            || manifest.get("dropped").and_then(serde_json::Value::as_u64) != Some(0)
        {
            return Err("actual corpus capture changed count or dropped exchanges".into());
        }
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
        manifest
            .as_object_mut()
            .ok_or("capture manifest is not an object")?
            .insert("calendarContract".into(), calendar);
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let hashes = Command::new("node").args(["-e", "const fs=require('node:fs'),crypto=require('node:crypto');const hash=p=>crypto.createHash('sha256').update(fs.readFileSync(p)).digest('hex');console.log(JSON.stringify({node:process.version,generatorSha256:hash(process.argv[1]),wasmSha256:hash(process.argv[2])}));"])
            .arg(core.join("tools/contract-corpus/generate.cjs"))
            .arg(core.join("server/wasm/dav-parse.wasm")).output().map_err(|e|e.to_string())?;
        if !hashes.status.success() {
            return Err("actual source provenance hashing failed".into());
        }
        let hashes: serde_json::Value =
            serde_json::from_slice(&hashes.stdout).map_err(|e| e.to_string())?;
        if hashes.get("wasmSha256").and_then(serde_json::Value::as_str)
            != Some("040738a438de8241bd2cd1558f2b06beaa32c5c9be01c30f09cd87cb2e978575")
        {
            return Err("actual core WASM provenance mismatch".into());
        }
        let provenance = serde_json::json!({"core":include_str!("../../../../contracts/http/core.ref").trim(),"node":hashes.get("node"),"generatorSha256":hashes.get("generatorSha256"),"wasmSha256":hashes.get("wasmSha256"),"generator":"actual core tools/contract-corpus/generate.cjs","hostInput":{"module":"server/inference-budget.cjs","collaborator":"readMeminfo","memTotalKib":8388608},"exchanges":193,"dropped":0});
        std::fs::write(
            Path::new(out).join("SOURCE_ORACLE.json"),
            serde_json::to_vec_pretty(&provenance).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        return Ok(());
    }
    // Replacing this process preserves the actual Node launcher's signal and child-reap handlers.
    let error = Command::new("node").arg(&wrapper).exec();
    let _ = std::fs::remove_dir_all(&directory);
    Err(error.to_string())
}
fn prepare(
    core: &Path,
    directory: &Path,
    entry: &str,
    argv: &[String],
    meminfo: &str,
) -> Result<PathBuf, String> {
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
        .replace("__TEMP__", &js(&directory.to_string_lossy())?);
    std::fs::write(&wrapper, source).map_err(|e| e.to_string())?;
    Ok(wrapper)
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
