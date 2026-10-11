//! Five new production mutations, each executed once from a clean committed snapshot.
//! Compiler/setup/deadline failures do not count as semantic kills.
#[path = "support/diagnose_process.rs"]
mod process;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
const DOCKER: &str = "bins/noevia-model-manager/src/diagnose_docker.rs";
const LIB: &str = "bins/noevia-model-manager/src/lib.rs";
const MAIN: &str = "bins/noevia-model-manager/src/main.rs";
struct Case {
    id: &'static str,
    source: &'static str,
    before: &'static str,
    after: &'static str,
    test_path: &'static str,
    target: &'static str,
    test: &'static str,
    assertion: &'static str,
    filtered: usize,
    replay: bool,
}
fn cases() -> [Case; 5] {
    [
        Case { id:"omit-since",source:DOCKER,before:"Self::Logs { id, since } => format!(\"/containers/{}/logs?stdout=true&stderr=true&timestamps=false&follow=false&tail=40000&since={since}\",identifier(id)?),",after:"Self::Logs { id, since } => { let _ = since; format!(\"/containers/{}/logs?stdout=true&stderr=true&timestamps=false&follow=false&tail=40000\",identifier(id)?) },",test_path:"bins/noevia-model-manager/tests/diagnose_transport.rs",target:"diagnose_transport",test:"exact_current_run_wire_and_split_utf8",assertion:"assert_eq!(requests[1].lines().next(),",filtered:7,replay:false },
        Case { id:"tail-400",source:DOCKER,before:"tail=40000&since={since}",after:"tail=400&since={since}",test_path:"bins/noevia-model-manager/tests/diagnose_transport.rs",target:"diagnose_transport",test:"exact_current_run_wire_and_split_utf8",assertion:"assert_eq!(requests[1].lines().next(),",filtered:7,replay:false },
        Case { id:"early-permit-release",source:DOCKER,before:"            task_context.cleanup().await;\n            drop(permit);",after:"            drop(permit);\n            task_context.cleanup().await;",test_path:"bins/noevia-model-manager/tests/diagnose_lifecycle.rs",target:"diagnose_lifecycle",test:"timed_out_response_retains_join_and_shutdown_until_driver_drop_completes",assertion:"assert_eq!(retained, 3);",filtered:4,replay:false },
        Case { id:"python-fallback",source:LIB,before:"diagnose_docker::Outcome::Refused => detail(\n                    StatusCode::SERVICE_UNAVAILABLE,\n                    \"Native backend diagnosis is unavailable\",\n                ),",after:"diagnose_docker::Outcome::Refused => dispatch(state, req).await,",test_path:"bins/noevia-model-manager/tests/diagnose_route.rs",target:"diagnose_route",test:"native_failure_never_uses_python_fallback",assertion:"assert_eq!(\n        (\n            status.as_u16(),",filtered:4,replay:false },
        Case { id:"child-confirmation",source:MAIN,before:"\"NOEVIA_NATIVE_MODEL_MANAGER_DIAGNOSE_CONFIRMED\",\n            if diagnose_mode == \"rust\" { \"1\" } else { \"0\" },",after:"\"NOEVIA_NATIVE_MODEL_MANAGER_DIAGNOSE_CONFIRMED\",\n            \"0\",",test_path:"tools/replay/src/bin/diagnose-contracts.rs",target:"diagnose-contracts",test:"tests::native_child_confirmation_semantics",assertion:"assert_eq!(marker, &json!(\"1\"));",filtered:3,replay:true },
    ]
}
fn read(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() > cap {
        return Err("input cap".into());
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > cap {
        return Err("input cap".into());
    }
    Ok(bytes)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn binary_hash(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() > 128 * 1024 * 1024 {
        return Err("binary cap".into());
    }
    let mut digest = Sha256::new();
    let mut count = 0usize;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        count = count
            .checked_add(n)
            .filter(|n| *n <= 128 * 1024 * 1024)
            .ok_or("binary cap")?;
        digest.update(buffer.get(..n).ok_or("binary read count")?);
    }
    Ok(format!("{:x}", digest.finalize()))
}
struct Restore {
    path: PathBuf,
    bytes: Vec<u8>,
}
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = fs::write(&self.path, &self.bytes);
    }
}
fn location(source: &str, case: &Case) -> Result<(usize, usize), String> {
    let name = case.test.rsplit("::").next().ok_or("test name")?;
    let start = source.find(&format!("fn {name}(")).ok_or("test absent")?;
    let region = source.get(start..).ok_or("test region")?;
    let region = region
        .get(
            ..region
                .find("\n}")
                .or_else(|| region.find("\n    }"))
                .ok_or("test end")?,
        )
        .ok_or("test end region")?;
    if region.matches(case.assertion).count() != 1 {
        return Err("assertion not unique within test".into());
    }
    let offset = start + region.find(case.assertion).ok_or("assertion absent")?;
    let prefix = source.get(..offset).ok_or("assertion prefix")?;
    let indent = prefix.rsplit('\n').next().ok_or("assertion indent")?;
    if !indent.bytes().all(|b| b == b' ') {
        return Err("assertion dialect".into());
    }
    Ok((
        prefix.bytes().filter(|b| *b == b'\n').count() + 1,
        indent.len() + 1,
    ))
}
fn semantic(
    code: Option<i32>,
    out: &str,
    err: &str,
    case: &Case,
    location: (usize, usize),
    root: &Path,
    target: &Path,
) -> bool {
    let (line, column) = location;
    if code != Some(101) || out.len() > 4 * 1024 * 1024 || err.len() > 64 * 1024 {
        return false;
    }
    let t = regex::escape(case.test);
    let p = regex::escape(case.test_path);
    let stdout=format!("\\A\\nrunning 1 test\\ntest {t} \\.\\.\\. FAILED\\n\\nfailures:\\n\\n---- {t} stdout ----\\n\\nthread '{t}' \\([0-9]{{1,20}}\\) panicked at {p}:{line}:{column}:\\nassertion `left == right` failed\\n  left: [^\\r\\n]+\\n right: [^\\r\\n]+\\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\\n\\n\\nfailures:\\n    {t}\\n\\ntest result: FAILED\\. 0 passed; 1 failed; 0 ignored; 0 measured; {} filtered out; finished in [0-9]+\\.[0-9]+s\\n\\n\\z",case.filtered);
    let (package, crate_path, running, footer, binary) = if case.replay {
        (
            "replay",
            "tools/replay",
            format!("unittests src/bin/{}.rs", case.target),
            "--bin diagnose-contracts",
            "diagnose_contracts",
        )
    } else {
        (
            "noevia-model-manager",
            "bins/noevia-model-manager",
            format!("tests/{}.rs", case.target),
            "",
            case.target,
        )
    };
    let footer = if case.replay {
        format!("error: test failed, to rerun pass `-p {package} {footer}`")
    } else {
        format!(
            "error: test failed, to rerun pass `-p {package} --test {}`",
            case.target
        )
    };
    let stderr=format!("\\A(?:   Compiling {package} v0\\.1\\.0 \\({}\\)\\n)?    Finished `test` profile \\[unoptimized \\+ debuginfo\\] target\\(s\\) in [0-9]+\\.[0-9]+s\\n     Running {} \\({}/debug/deps/{}-[0-9a-f]{{16}}\\)\\n{}\\n\\z",regex::escape(&root.join(crate_path).display().to_string()),regex::escape(&running),regex::escape(&target.display().to_string()),regex::escape(binary),regex::escape(&footer));
    regex::Regex::new(&stdout).is_ok_and(|r| r.is_match(out))
        && regex::Regex::new(&stderr).is_ok_and(|r| r.is_match(err))
}
fn git(args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = Command::new("git");
    command.args(args);
    let output = process::bounded(command, Vec::new(), Duration::from_secs(10))?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err("git snapshot failure".into());
    }
    Ok(output.stdout)
}
fn cargo(case: &Case, build: bool) -> Command {
    let mut command = Command::new("cargo");
    command.args([
        if build { "build" } else { "test" },
        "--offline",
        "--locked",
        "-p",
        if case.replay && !build {
            "replay"
        } else {
            "noevia-model-manager"
        },
    ]);
    if build {
        command.args([
            "--features",
            "native-diagnose",
            "--bin",
            "noevia-model-manager",
        ]);
    } else if case.replay {
        command.args(["--bin", case.target, case.test, "--", "--exact"]);
    } else {
        command.args([
            "--features",
            "native-diagnose",
            "--test",
            case.target,
            case.test,
            "--",
            "--exact",
        ]);
    }
    command.env_remove("RUST_BACKTRACE");
    command
}
fn execute() -> Result<(), String> {
    if !git(&["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        return Err("clean committed snapshot required".into());
    }
    let root = std::env::current_dir().map_err(|e| e.to_string())?;
    let snapshot = String::from_utf8(git(&["rev-parse", "HEAD", "HEAD^{tree}"])?)
        .map_err(|e| e.to_string())?;
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .ok_or("explicit owned CARGO_TARGET_DIR required")?;
    let target = if target.is_absolute() {
        target
    } else {
        root.join(target)
    };
    let evidence = std::env::var_os("NOEVIA_DIAGNOSE_MUTATION_EVIDENCE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/s1-diagnose-wave-b-mutations"));
    if evidence.exists() {
        return Err("fresh mutation evidence directory required; no replay execution".into());
    }
    fs::create_dir(&evidence).map_err(|e| e.to_string())?;
    let mut certificate = Vec::new();
    for case in cases() {
        let original = read(Path::new(case.source), 4 * 1024 * 1024)?;
        let tests = read(Path::new(case.test_path), 4 * 1024 * 1024)?;
        if git(&["show", &format!("HEAD:{}", case.source)])? != original
            || git(&["show", &format!("HEAD:{}", case.test_path)])? != tests
        {
            return Err("source must match committed snapshot".into());
        }
        let text = std::str::from_utf8(&original).map_err(|e| e.to_string())?;
        if text.matches(case.before).count() != 1 {
            return Err(format!("mutant location not unique: {}", case.id));
        }
        let (line, column) = location(
            std::str::from_utf8(&tests).map_err(|e| e.to_string())?,
            &case,
        )?;
        let mut mutant = text.replacen(case.before, case.after, 1);
        if case.id == "python-fallback" {
            let needle = ".request(state, name, deadline)";
            if mutant.matches(needle).count() != 1 {
                return Err("fallback state ownership location".into());
            }
            mutant = mutant.replacen(needle, ".request(state.clone(), name, deadline)", 1);
        }
        fs::write(
            evidence.join(format!("{}.production", case.id)),
            mutant.as_bytes(),
        )
        .map_err(|e| e.to_string())?;
        fs::write(evidence.join(format!("{}.tests", case.id)), &tests)
            .map_err(|e| e.to_string())?;
        let restore = Restore {
            path: PathBuf::from(case.source),
            bytes: original.clone(),
        };
        fs::write(case.source, mutant.as_bytes()).map_err(|e| e.to_string())?;
        let build_hash = if case.replay {
            let output =
                process::bounded(cargo(&case, true), Vec::new(), Duration::from_secs(120))?;
            fs::write(
                evidence.join(format!("{}.build.stdout", case.id)),
                &output.stdout,
            )
            .map_err(|e| e.to_string())?;
            fs::write(
                evidence.join(format!("{}.build.stderr", case.id)),
                &output.stderr,
            )
            .map_err(|e| e.to_string())?;
            if !output.status.success() {
                return Err("mutant manager compilation is not a kill".into());
            }
            Some(binary_hash(&target.join("debug/noevia-model-manager"))?)
        } else {
            None
        };
        let output = process::bounded(cargo(&case, false), Vec::new(), Duration::from_secs(120))?;
        fs::write(evidence.join(format!("{}.stdout", case.id)), &output.stdout)
            .map_err(|e| e.to_string())?;
        fs::write(evidence.join(format!("{}.stderr", case.id)), &output.stderr)
            .map_err(|e| e.to_string())?;
        drop(restore);
        let out = std::str::from_utf8(&output.stdout).map_err(|e| e.to_string())?;
        let err = std::str::from_utf8(&output.stderr).map_err(|e| e.to_string())?;
        let killed = semantic(
            output.status.code(),
            out,
            err,
            &case,
            (line, column),
            &root,
            &target,
        );
        let running = err
            .lines()
            .find(|s| s.starts_with("     Running "))
            .and_then(|s| s.rsplit_once(" ("))
            .and_then(|(_, p)| p.strip_suffix(')'))
            .ok_or("test binary identity absent")?;
        let binary = Path::new(running);
        if binary.parent() != Some(target.join("debug/deps").as_path()) {
            return Err("test binary escaped owned target".into());
        }
        certificate.push(json!({"id":case.id,"snapshot":snapshot,"checker_sha256":hash(include_bytes!("check-diagnose-wave-b-mutations.rs")),"production_path":case.source,"original_sha256":hash(&original),"mutant_sha256":hash(mutant.as_bytes()),"test_path":case.test_path,"tests_sha256":hash(&tests),"assertion_line":line,"assertion_column":column,"status":output.status.code(),"stdout_sha256":hash(&output.stdout),"stderr_sha256":hash(&output.stderr),"test_binary_sha256":binary_hash(binary)?,"manager_binary_sha256":build_hash,"semantic_kill":killed}));
        fs::write(
            evidence.join("certificate.json"),
            serde_json::to_vec_pretty(&certificate).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if !killed {
            return Err(format!("unqualified mutation outcome: {} (never count setup/compiler/cap/deadline failures)",case.id));
        }
    }
    if !git(&["status", "--porcelain", "--untracked-files=no"])?.is_empty() {
        return Err("snapshot restoration mismatch".into());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&certificate).map_err(|e| e.to_string())?
    );
    Ok(())
}
fn main() -> Result<(), String> {
    if std::env::args_os().count() != 2
        || std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new("--execute"))
    {
        return Err("explicit --execute once from final snapshot required".into());
    }
    execute()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn production_plan_binds_unique_current_locations() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for case in cases() {
            let source = read(&root.join(case.source), 4 * 1024 * 1024).unwrap();
            let source = std::str::from_utf8(&source).unwrap();
            assert_eq!(source.matches(case.before).count(), 1, "{}", case.id);
            if case.id == "python-fallback" {
                assert_eq!(source.matches(".request(state, name, deadline)").count(), 1);
            }
            let tests = read(&root.join(case.test_path), 4 * 1024 * 1024).unwrap();
            let (line, column) = location(std::str::from_utf8(&tests).unwrap(), &case).unwrap();
            assert!(line > 0 && matches!(column, 5 | 9));
        }
    }
    #[test]
    fn strict_complete_transcript_rejects_setup_caps_and_wrong_locations() {
        let case = cases().into_iter().next().unwrap();
        let out=format!("\nrunning 1 test\ntest {t} ... FAILED\n\nfailures:\n\n---- {t} stdout ----\n\nthread '{t}' (123) panicked at {p}:104:5:\nassertion `left == right` failed\n  left: Some(\"wrong\")\n right: Some(\"expected\")\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\nfailures:\n    {t}\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 7 filtered out; finished in 0.01s\n\n",t=case.test,p=case.test_path);
        let err="   Compiling noevia-model-manager v0.1.0 (/owned/bins/noevia-model-manager)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.12s\n     Running tests/diagnose_transport.rs (/target/debug/deps/diagnose_transport-0123456789abcdef)\nerror: test failed, to rerun pass `-p noevia-model-manager --test diagnose_transport`\n";
        let check = |code, out: &str, err: &str| {
            semantic(
                code,
                out,
                err,
                &case,
                (104, 5),
                Path::new("/owned"),
                Path::new("/target"),
            )
        };
        assert!(check(Some(101), &out, err));
        for bad in [
            format!("{out}SETUP\n"),
            format!("SETUP\n{out}"),
            format!("{out}{out}"),
            out.replace(":104:5:", ":104:3:"),
            out.replace("  left:", "deadline cap\n  left:"),
            out.replace("assertion `left == right` failed", "called unwrap on Err"),
            out.replace("7 filtered out", "6 filtered out"),
            out.trim_end().to_owned(),
        ] {
            assert!(!check(Some(101), &bad, err));
        }
        for bad in [
            format!("{err}SETUP\n"),
            format!("SETUP\n{err}"),
            format!("{err}{err}"),
            err.replace("error: test failed", "error: could not compile"),
            err.replace("    Finished", "output cap\n    Finished"),
            err.trim_end().to_owned(),
        ] {
            assert!(!check(Some(101), &out, &bad));
        }
        for code in [None, Some(0), Some(1)] {
            assert!(!check(code, &out, err));
        }
    }
}
