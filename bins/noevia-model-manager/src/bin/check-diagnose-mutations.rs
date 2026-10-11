//! Each production mutant runs exactly once, in the assigned private checkout.
//! A kill requires status 101 at the source-bound assertion and terminal test footer.
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
const SOURCE: &str = "bins/noevia-model-manager/src/diagnose.rs";
const TEST: &str = "bins/noevia-model-manager/tests/diagnose.rs";
const EVIDENCE: &str = "/tmp/s1-backend-diagnose-mutations";
fn read(path: &Path) -> Result<Vec<u8>, String> {
    let mut f = fs::File::open(path).map_err(|e| e.to_string())?;
    if f.metadata().map_err(|e| e.to_string())?.len() > 4 * 1024 * 1024 {
        return Err("source cap".into());
    }
    let mut bytes = Vec::new();
    f.by_ref()
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("source cap".into());
    }
    Ok(bytes)
}
fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn binary_hash(stderr: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(stderr).map_err(|e| e.to_string())?;
    let prefix = "     Running tests/diagnose.rs (";
    let paths = text
        .lines()
        .filter_map(|s| s.strip_prefix(prefix).and_then(|s| s.strip_suffix(')')))
        .collect::<Vec<_>>();
    if paths.len() != 1 {
        return Err("exact mutant test binary identity absent".into());
    }
    let path = *paths.first().ok_or("test binary path absent")?;
    let suffix = path
        .strip_prefix("/tmp/s1-backend-diagnose-target/debug/deps/diagnose-")
        .ok_or("test binary escaped owned target")?;
    if suffix.len() != 16 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("test binary path dialect".into());
    }
    let mut f = fs::File::open(path).map_err(|e| e.to_string())?;
    if f.metadata().map_err(|e| e.to_string())?.len() > 128 * 1024 * 1024 {
        return Err("binary hash cap".into());
    }
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    let mut count = 0usize;
    loop {
        let n = f.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        count = count
            .checked_add(n)
            .filter(|n| *n <= 128 * 1024 * 1024)
            .ok_or("binary hash cap")?;
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
fn assertion_line(source: &str, test: &str) -> Result<usize, String> {
    let start = source.find(&format!("fn {test}(")).ok_or("test absent")?;
    let region = source.get(start..).ok_or("test region")?;
    let region = region
        .get(..region.find("\n}\n").ok_or("test end")?)
        .ok_or("test region end")?;
    if region.matches("assert_eq!").count() != 1 {
        return Err("assertion not unique".into());
    }
    let offset = start + region.find("assert_eq!").ok_or("assertion absent")?;
    Ok(source
        .get(..offset)
        .ok_or("line region")?
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
        + 1)
}
fn semantic(code: Option<i32>, out: &str, err: &str, test: &str, line: usize) -> bool {
    if code != Some(101)
        || !out.lines().any(|s| s == "running 1 test")
        || !out.contains("test result: FAILED. 0 passed; 1 failed;")
        || out.matches(" panicked at ").count() != 1
        || err.lines().any(|s| {
            s.starts_with("error[")
                || (s.starts_with("error:") && !s.starts_with("error: test failed, to rerun pass"))
        })
    {
        return false;
    }
    let mut failed = out.lines().filter_map(|s| {
        s.strip_prefix("test ")
            .and_then(|s| s.strip_suffix(" ... FAILED"))
            .or_else(|| s.strip_suffix(" --- FAILED"))
    });
    if failed.next() != Some(test)
        || failed.next().is_some()
        || !out.contains(&format!("---- {test} stdout ----"))
    {
        return false;
    }
    let mut lines = out.lines();
    while let Some(s) = lines.next() {
        if let Some(tail) = s.strip_prefix(&format!("thread '{test}'")) {
            let location = tail.strip_prefix(" panicked at ").or_else(|| {
                let (id, loc) = tail.strip_prefix(" (")?.split_once(") panicked at ")?;
                (!id.is_empty() && id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit()))
                    .then_some(loc)
            });
            let exact = location
                .and_then(|s| s.strip_prefix(&format!("{TEST}:{line}:")))
                .and_then(|s| s.strip_suffix(':'))
                .is_some_and(|c| {
                    !c.is_empty() && c.len() <= 6 && c.bytes().all(|b| b.is_ascii_digit())
                });
            return exact
                && lines
                    .next()
                    .is_some_and(|s| s.starts_with("assertion `left == right` failed"));
        }
    }
    false
}
fn run() -> Result<(), String> {
    if std::env::args().nth(1).as_deref() != Some("--run-once") || std::env::args().count() != 2 {
        return Err("explicit --run-once required in the private assigned worktree".into());
    }
    let here = std::env::current_dir().map_err(|e| e.to_string())?;
    if here != Path::new("/Users/sebastiandalton/noevia-work/s1-backend-diagnose-rs") {
        return Err("mutation checkout identity mismatch".into());
    }
    // An existing certificate prevents accidental reruns of already passing mutants.
    fs::create_dir(EVIDENCE)
        .map_err(|e| format!("fresh mutation evidence directory required: {e}"))?;
    let original = read(Path::new(SOURCE))?;
    let source = std::str::from_utf8(&original).map_err(|e| e.to_string())?;
    let tests = read(Path::new(TEST))?;
    let test_text = std::str::from_utf8(&tests).map_err(|e| e.to_string())?;
    let fixtures = read(Path::new(
        "bins/noevia-model-manager/tests/fixtures/diagnose-python.v1.json",
    ))?;
    let mutations = [
        (
            "priority",
            "o.hits.iter().position(|hits| !hits.is_empty())",
            "o.hits.iter().rposition(|hits| !hits.is_empty())",
            "priority_semantics",
        ),
        (
            "forced",
            "status == 0 || requested.contains(name)",
            "status == 0",
            "forced_semantics",
        ),
        (
            "slot",
            "*old = f;",
            "*old = f; failures.sort_by(|a,b| b.model.cmp(a.model));",
            "slot_semantics",
        ),
        (
            "distinct-port",
            "port != f.port",
            "port == f.port",
            "distinct_port_semantics",
        ),
        (
            "case",
            "line.contains(\"model loaded\")",
            "line.to_lowercase().contains(\"model loaded\")",
            "case_sensitive_success_semantics",
        ),
        (
            "redaction",
            "let safe = self.redact(line, work)?;",
            "let safe = (*line).to_owned();",
            "redaction_semantics",
        ),
        (
            "crossline",
            "let suppress = whole != per_line;",
            "let suppress = false;",
            "crossline_semantics",
        ),
    ];
    let mut certificate = Vec::new();
    for (id, before, after, test) in mutations {
        if source.matches(before).count() != 1 {
            return Err(format!("nonunique mutation {id}"));
        }
        let mutant = source.replace(before, after);
        let line = assertion_line(test_text, test)?;
        let restore = Restore {
            path: PathBuf::from(SOURCE),
            bytes: original.clone(),
        };
        fs::write(SOURCE, &mutant).map_err(|e| e.to_string())?;
        let mut c = Command::new(
            "/Users/sebastiandalton/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin/cargo",
        );
        c.env_clear().env("PATH","/Users/sebastiandalton/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin:/usr/bin:/bin").env("HOME","/Users/sebastiandalton").env("CARGO_HOME","/Users/sebastiandalton/.cargo").env("CARGO_TARGET_DIR","/tmp/s1-backend-diagnose-target").args(["test","--offline","--locked","-p","noevia-model-manager","--test","diagnose",test,"--","--exact"]);
        let outcome = process::bounded(c, Vec::new(), Duration::from_secs(60));
        fs::write(SOURCE, &restore.bytes).map_err(|e| e.to_string())?;
        drop(restore);
        let o = outcome?;
        fs::write(Path::new(EVIDENCE).join(format!("{id}.stdout")), &o.stdout)
            .map_err(|e| e.to_string())?;
        fs::write(Path::new(EVIDENCE).join(format!("{id}.stderr")), &o.stderr)
            .map_err(|e| e.to_string())?;
        let killed = semantic(
            o.status.code(),
            &String::from_utf8_lossy(&o.stdout),
            &String::from_utf8_lossy(&o.stderr),
            test,
            line,
        );
        let row = json!({"id":id,"test":test,"assertion_path":TEST,"assertion_line":line,"status":o.status.code(),"semantic_kill":killed,"production_sha256":hash(&original),"mutant_sha256":hash(mutant.as_bytes()),"test_binary_sha256":binary_hash(&o.stderr)?,"tests_sha256":hash(&tests),"fixture_sha256":hash(&fixtures),"checker_sha256":hash(include_bytes!("check-diagnose-mutations.rs")),"stdout_sha256":hash(&o.stdout),"stderr_sha256":hash(&o.stderr)});
        fs::write(
            Path::new(EVIDENCE).join(format!("{id}.json")),
            serde_json::to_vec_pretty(&row).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        certificate.push(row);
        if !killed {
            return Err(format!(
                "{id} did not reach its bound semantic assertion; raw evidence retained"
            ));
        }
        println!("{id}: semantic assertion killed production mutant once");
    }
    fs::write(
        Path::new(EVIDENCE).join("certificate.json"),
        serde_json::to_vec_pretty(&certificate).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
fn main() -> Result<(), String> {
    run()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifier_requires_exact_assertion_terminal_footer_and_status() {
        let out=format!("running 1 test\ntest target ... FAILED\n---- target stdout ----\nthread 'target' (123) panicked at {TEST}:12:3:\nassertion `left == right` failed\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 1 filtered out;\n");
        assert!(semantic(Some(101), &out, "", "target", 12));
        for (code, text, err, name, line) in [
            (Some(0), out.clone(), "", "target", 12),
            (
                Some(101),
                out.replace("test result: FAILED.", "test result: ok."),
                "",
                "target",
                12,
            ),
            (
                Some(101),
                out.replace("assertion `left == right` failed", "called unwrap on Err"),
                "",
                "target",
                12,
            ),
            (
                Some(101),
                out.clone(),
                "error: could not compile",
                "target",
                12,
            ),
            (Some(101), out.clone(), "", "other", 12),
            (Some(101), out.clone(), "", "target", 13),
        ] {
            assert!(!semantic(code, &text, err, name, line));
        }
    }
}
