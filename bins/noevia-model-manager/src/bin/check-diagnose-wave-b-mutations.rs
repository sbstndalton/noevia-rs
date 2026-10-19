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
        Case {
            id: "omit-since",
            source: DOCKER,
            before: "Self::Logs { id, since } => format!(\"/containers/{}/logs?stdout=true&stderr=true&timestamps=false&follow=false&tail=40000&since={since}\",identifier(id)?),",
            after: "Self::Logs { id, since } => { let _ = since; format!(\"/containers/{}/logs?stdout=true&stderr=true&timestamps=false&follow=false&tail=40000\",identifier(id)?) },",
            test_path: "bins/noevia-model-manager/tests/diagnose_transport.rs",
            target: "diagnose_transport",
            test: "exact_current_run_wire_and_split_utf8",
            assertion: "assert_eq!(requests[1].lines().next(),",
            filtered: 7,
            replay: false,
        },
        Case {
            id: "tail-400",
            source: DOCKER,
            before: "tail=40000&since={since}",
            after: "tail=400&since={since}",
            test_path: "bins/noevia-model-manager/tests/diagnose_transport.rs",
            target: "diagnose_transport",
            test: "exact_current_run_wire_and_split_utf8",
            assertion: "assert_eq!(requests[1].lines().next(),",
            filtered: 7,
            replay: false,
        },
        Case {
            id: "early-permit-release",
            source: DOCKER,
            before: "            task_context.cleanup().await;\n            drop(permit);",
            after: "            drop(permit);\n            task_context.cleanup().await;",
            test_path: "bins/noevia-model-manager/tests/diagnose_lifecycle.rs",
            target: "diagnose_lifecycle",
            test: "timed_out_response_retains_join_and_shutdown_until_driver_drop_completes",
            assertion: "assert_eq!(retained, 3);",
            filtered: 4,
            replay: false,
        },
        Case {
            id: "python-fallback",
            source: LIB,
            before: "diagnose_docker::Outcome::Refused => detail(\n                    StatusCode::SERVICE_UNAVAILABLE,\n                    \"Native backend diagnosis is unavailable\",\n                ),",
            after: "diagnose_docker::Outcome::Refused => dispatch(state, req).await,",
            test_path: "bins/noevia-model-manager/tests/diagnose_route.rs",
            target: "diagnose_route",
            test: "native_failure_never_uses_python_fallback",
            assertion: "assert_eq!(\n        (\n            status.as_u16(),",
            filtered: 4,
            replay: false,
        },
        Case {
            id: "mandatory-capability",
            source: MAIN,
            before: "if !cfg!(feature = \"native-routing\") {",
            after: "if false {",
            test_path: MAIN,
            target: "noevia-model-manager",
            test: "native_capability_tests::mandatory_capability_is_required_before_startup",
            assertion: "assert_eq!(\n            super::require_native_capability().is_err(),",
            // Verified native-diagnose-only --list: capability and legacy-oracle tests.
            filtered: 1,
            replay: false,
        },
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
    let stdout = format!(
        "\\A\\nrunning 1 test\\ntest {t} \\.\\.\\. FAILED\\n\\nfailures:\\n\\n---- {t} stdout ----\\n\\nthread '{t}' \\([0-9]{{1,20}}\\) panicked at {p}:{line}:{column}:\\nassertion `left == right` failed\\n  left: [^\\r\\n]+\\n right: [^\\r\\n]+\\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\\n\\n\\nfailures:\\n    {t}\\n\\ntest result: FAILED\\. 0 passed; 1 failed; 0 ignored; 0 measured; {} filtered out; finished in [0-9]+\\.[0-9]+s\\n\\n\\z",
        case.filtered
    );
    let (package, crate_path, running, footer, binary) = if case.id == "mandatory-capability" {
        (
            "noevia-model-manager",
            "bins/noevia-model-manager",
            "unittests src/main.rs".into(),
            "--bin noevia-model-manager",
            "noevia_model_manager",
        )
    } else if case.replay {
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
    let footer = if case.replay || case.id == "mandatory-capability" {
        format!("error: test failed, to rerun pass `-p {package} {footer}`")
    } else {
        format!(
            "error: test failed, to rerun pass `-p {package} --test {}`",
            case.target
        )
    };
    let stderr = format!(
        "\\A(?:   Compiling {package} v0\\.1\\.0 \\({}\\)\\n)?    Finished `test` profile \\[unoptimized \\+ debuginfo\\] target\\(s\\) in [0-9]+\\.[0-9]+s\\n     Running {} \\({}/debug/deps/{}-[0-9a-f]{{16}}\\)\\n{}\\n\\z",
        regex::escape(&root.join(crate_path).display().to_string()),
        regex::escape(&running),
        match target.strip_prefix(root) {
            Ok(relative) if !relative.as_os_str().is_empty() => format!("(?:{}|{})", regex::escape(&target.display().to_string()), regex::escape(&relative.display().to_string())),
            _ => regex::escape(&target.display().to_string()),
        },
        regex::escape(binary),
        regex::escape(&footer)
    );
    regex::Regex::new(&stdout).is_ok_and(|r| r.is_match(out))
        && regex::Regex::new(&stderr).is_ok_and(|r| r.is_match(err))
}
fn case_binary(case: &Case) -> &str {
    if case.id == "mandatory-capability" {
        "noevia_model_manager"
    } else if case.replay {
        "diagnose_contracts"
    } else {
        case.target
    }
}
fn resolve_test_binary(
    rendered: &str,
    root: &Path,
    target: &Path,
    prefix: &str,
) -> Result<PathBuf, String> {
    use std::path::Component;
    if rendered.ends_with('/')
        || rendered.len() > 8192
        || !root.is_absolute()
        || !target.is_absolute()
        || [root, target].iter().any(|p| {
            p.components()
                .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
        })
        || rendered
            .split('/')
            .any(|part| part.is_empty() && !rendered.starts_with('/'))
        || rendered.contains("//")
    {
        return Err("test binary path malformed".into());
    }
    let path = Path::new(rendered);
    if path
        .components()
        .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
        || rendered.split('/').any(|c| c == "." || c == "..")
    {
        return Err("test binary path traversal".into());
    }
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    if resolved.parent() != Some(target.join("debug/deps").as_path()) {
        return Err("test binary escaped owned target".into());
    }
    let name = resolved
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or("test binary name")?;
    let suffix = name
        .strip_prefix(prefix)
        .and_then(|v| v.strip_prefix('-'))
        .ok_or("wrong test binary")?;
    if suffix.len() != 16
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("test binary hash suffix malformed".into());
    }
    Ok(resolved)
}
fn reject_binary_symlinks(binary: &Path, target: &Path) -> Result<(), String> {
    for path in [
        target.to_path_buf(),
        target.join("debug"),
        target.join("debug/deps"),
        binary.to_path_buf(),
    ] {
        let metadata = fs::symlink_metadata(path).map_err(|_| "owned binary metadata absent")?;
        if metadata.file_type().is_symlink() {
            return Err("owned binary symlink refused".into());
        }
    }
    Ok(())
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
fn pin_owned_cargo_output(command: &mut Command) {
    command.env("CARGO_TERM_COLOR", "never");
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
    } else if case.id == "mandatory-capability" {
        command.args([
            "--features",
            "native-diagnose",
            "--bin",
            case.target,
            case.test,
            "--",
            "--exact",
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
    pin_owned_cargo_output(&mut command);
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
        let binary = resolve_test_binary(running, &root, &target, case_binary(&case))?;
        reject_binary_symlinks(&binary, &target)?;
        certificate.push(json!({"id":case.id,"snapshot":snapshot,"checker_sha256":hash(include_bytes!("check-diagnose-wave-b-mutations.rs")),"production_path":case.source,"original_sha256":hash(&original),"mutant_sha256":hash(mutant.as_bytes()),"test_path":case.test_path,"tests_sha256":hash(&tests),"assertion_line":line,"assertion_column":column,"status":output.status.code(),"stdout_sha256":hash(&output.stdout),"stderr_sha256":hash(&output.stderr),"test_binary_sha256":binary_hash(&binary)?,"manager_binary_sha256":build_hash,"semantic_kill":killed}));
        fs::write(
            evidence.join("certificate.json"),
            serde_json::to_vec_pretty(&certificate).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        if !killed {
            return Err(format!(
                "unqualified mutation outcome: {} (never count setup/compiler/cap/deadline failures)",
                case.id
            ));
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
    fn current_mandatory_capability_inventory_is_exact() -> Result<(), String> {
        let case = cases()
            .into_iter()
            .last()
            .ok_or("fixed final case absent")?;
        let out = format!(
            "\nrunning 1 test\ntest {t} ... FAILED\n\nfailures:\n\n---- {t} stdout ----\n\nthread '{t}' (123) panicked at {p}:100:9:\nassertion `left == right` failed\n  left: false\n right: true\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\nfailures:\n    {t}\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.01s\n\n",
            t = case.test,
            p = case.test_path
        );
        let err = "    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.12s\n     Running unittests src/main.rs (/target/debug/deps/noevia_model_manager-0123456789abcdef)\nerror: test failed, to rerun pass `-p noevia-model-manager --bin noevia-model-manager`\n";
        let check = |out: &str, err: &str| {
            semantic(
                Some(101),
                out,
                err,
                &case,
                (100, 9),
                Path::new("/owned"),
                Path::new("/target"),
            )
        };
        assert!(check(&out, err));
        for bad in [
            out.replace("1 filtered out", "2 filtered out"),
            out.replace("1 filtered out", "3 filtered out"),
            out.replace("1 filtered out", "4 filtered out"),
            out.replace("1 filtered out", "6 filtered out"),
            out.replace("running 1 test", "running 2 tests"),
            format!("{out}extra test ... FAILED\n"),
        ] {
            assert!(!check(&bad, err));
        }
        for bad in [
            err.replace(
                "unittests src/main.rs",
                "unittests src/bin/diagnose-contracts.rs",
            ),
            err.replace("noevia_model_manager-", "diagnose_contracts-"),
            err.replace(
                "-p noevia-model-manager --bin noevia-model-manager",
                "-p replay --bin diagnose-contracts",
            ),
            err.replace("--bin noevia-model-manager", "--test noevia-model-manager"),
            format!("{err}{err}"),
            format!("{err}setup\n"),
        ] {
            assert!(!check(&out, &bad));
        }
        for bad in [
            out.replace(":100:9:", ":100:13:"),
            out.replace(case.test_path, "tools/replay/src/bin/diagnose-contracts.rs"),
        ] {
            assert!(!check(&bad, err));
        }
        Ok(())
    }
    #[test]
    fn production_plan_binds_unique_current_locations() -> Result<(), String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for case in cases() {
            let source = read(&root.join(case.source), 4 * 1024 * 1024)?;
            let source = std::str::from_utf8(&source).map_err(|e| e.to_string())?;
            assert_eq!(source.matches(case.before).count(), 1, "{}", case.id);
            if case.id == "python-fallback" {
                assert_eq!(source.matches(".request(state, name, deadline)").count(), 1);
            }
            let tests = read(&root.join(case.test_path), 4 * 1024 * 1024)?;
            let (line, column) = location(
                std::str::from_utf8(&tests).map_err(|e| e.to_string())?,
                &case,
            )?;
            assert!(line > 0);
            if case.id == "mandatory-capability" {
                assert_eq!(case.test_path, MAIN);
                assert_eq!(column, 9);
            } else {
                assert!(matches!(column, 5 | 9));
            }
        }
        Ok(())
    }

    #[test]
    fn mandatory_capability_qualifies_the_actual_two_test_harness_inventory() -> Result<(), String>
    {
        let case = cases()
            .into_iter()
            .find(|c| c.id == "mandatory-capability")
            .ok_or("mandatory capability case absent")?;
        let (line, column) = location(include_str!("../main.rs"), &case)?;
        // The native-diagnose-only binary has exactly two tests (recorded --list).
        // This production capability mutation must reach the owned boolean assertion.
        let out = format!(
            r#"
running 1 test
test {t} ... FAILED

failures:

---- {t} stdout ----

thread '{t}' (11394) panicked at {p}:{line}:{column}:
assertion `left == right` failed
  left: false
 right: true
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    {t}

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1 filtered out; finished in 0.00s

"#,
            t = case.test,
            p = case.test_path
        );
        let err = "   Compiling noevia-model-manager v0.1.0 (/owned/bins/noevia-model-manager)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 4.12s\n     Running unittests src/main.rs (/target/debug/deps/noevia_model_manager-0123456789abcdef)\nerror: test failed, to rerun pass `-p noevia-model-manager --bin noevia-model-manager`\n";
        let check = |out: &str, err: &str| {
            semantic(
                Some(101),
                out,
                err,
                &case,
                (line, column),
                Path::new("/owned"),
                Path::new("/target"),
            )
        };
        assert!(check(&out, err));
        for count in [0, 2, 4, 5, 6] {
            assert!(!check(
                &out.replace("1 filtered out", &format!("{count} filtered out")),
                err
            ));
        }
        assert!(!check(&format!("{out}SETUP\n"), err));
        assert!(!check(
            &out,
            &err.replace("    Finished", "deadline cap\n    Finished")
        ));
        assert!(!check(
            &out,
            &err.replace("error: test failed", "error: could not compile")
        ));
        Ok(())
    }

    #[test]
    fn strict_complete_transcript_rejects_setup_caps_and_wrong_locations() -> Result<(), String> {
        let case = cases()
            .into_iter()
            .next()
            .ok_or("fixed first case absent")?;
        let out = format!(
            "\nrunning 1 test\ntest {t} ... FAILED\n\nfailures:\n\n---- {t} stdout ----\n\nthread '{t}' (123) panicked at {p}:104:5:\nassertion `left == right` failed\n  left: Some(\"wrong\")\n right: Some(\"expected\")\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\nfailures:\n    {t}\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 7 filtered out; finished in 0.01s\n\n",
            t = case.test,
            p = case.test_path
        );
        let err = "   Compiling noevia-model-manager v0.1.0 (/owned/bins/noevia-model-manager)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.12s\n     Running tests/diagnose_transport.rs (/target/debug/deps/diagnose_transport-0123456789abcdef)\nerror: test failed, to rerun pass `-p noevia-model-manager --test diagnose_transport`\n";
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
        Ok(())
    }
    #[test]
    fn owned_cargo_output_overrides_inherited_color() -> Result<(), String> {
        let mut command = Command::new("cargo");
        command.env("CARGO_TERM_COLOR", "always");
        pin_owned_cargo_output(&mut command);
        for command in [
            command,
            cargo(
                &cases().into_iter().next().ok_or("fixed case absent")?,
                false,
            ),
            cargo(
                &cases().into_iter().next().ok_or("fixed case absent")?,
                true,
            ),
        ] {
            assert!(command
                .get_envs()
                .any(|(key, value)| key == "CARGO_TERM_COLOR"
                    && value == Some(std::ffi::OsStr::new("never"))));
        }
        Ok(())
    }
    #[test]
    fn exact_owned_binary_renderings_refuse_escape_and_malformed() -> Result<(), String> {
        let root = Path::new("/owned");
        let target = root.join("target");
        let expected = target.join("debug/deps/diagnose_transport-0123456789abcdef");
        for path in [
            "/owned/target/debug/deps/diagnose_transport-0123456789abcdef",
            "target/debug/deps/diagnose_transport-0123456789abcdef",
        ] {
            assert_eq!(
                resolve_test_binary(path, root, &target, "diagnose_transport")?,
                expected
            );
        }
        for path in [
            "../target/debug/deps/diagnose_transport-0123456789abcdef",
            "target/../target/debug/deps/diagnose_transport-0123456789abcdef",
            "./target/debug/deps/diagnose_transport-0123456789abcdef",
            "/other/target/debug/deps/diagnose_transport-0123456789abcdef",
            "target2/debug/deps/diagnose_transport-0123456789abcdef",
            "target/debug/deps/wrong-0123456789abcdef",
            "target/debug/deps/diagnose_transport-0123456789abcdeF",
            "target/debug/deps/diagnose_transport-0123456789abcdef.extra",
            "target//debug/deps/diagnose_transport-0123456789abcdef",
            "target/debug/deps/diagnose_transport-0123456789abcdef/",
            "",
        ] {
            assert!(resolve_test_binary(path, root, &target, "diagnose_transport").is_err());
        }
        let owned = tempfile::tempdir().map_err(|_| "fixture tempdir")?;
        let target = owned.path().join("target");
        fs::create_dir_all(target.join("debug/deps")).map_err(|_| "fixture directory")?;
        let binary = target.join("debug/deps/diagnose_transport-0123456789abcdef");
        fs::write(&binary, b"fixed").map_err(|_| "fixture binary")?;
        reject_binary_symlinks(&binary, &target)?;
        let link = target.join("debug/deps/diagnose_transport-fedcba9876543210");
        std::os::unix::fs::symlink(&binary, &link).map_err(|_| "fixture symlink")?;
        assert!(reject_binary_symlinks(&link, &target).is_err());
        Ok(())
    }
}
