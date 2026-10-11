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
    let prefix = source.get(..offset).ok_or("assertion prefix")?;
    if prefix.rsplit('\n').next() != Some("    ") {
        return Err("unsupported assertion column; expected 5".into());
    }
    Ok(source
        .get(..offset)
        .ok_or("line region")?
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
        + 1)
}
// Intentionally narrow libtest/Cargo dialect: every byte must belong to this
// single assertion transcript. Unknown diagnostics are qualification failures.
fn decimal(s: &str) -> bool {
    let Some((whole, fraction)) = s.split_once('.') else {
        return false;
    };
    !whole.is_empty()
        && !fraction.is_empty()
        && whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
}
fn semantic(code: Option<i32>, out: &str, err: &str, test: &str, line: usize) -> bool {
    if code != Some(101) || out.len() > 4 * 1024 * 1024 || err.len() > 4 * 1024 * 1024 {
        return false;
    }
    let pattern = format!(
        "\\A\\nrunning 1 test\\ntest {t} \\.\\.\\. FAILED\\n\\nfailures:\\n\\n---- {t} stdout ----\\n\\nthread '{t}' \\([0-9]{{1,20}}\\) panicked at {p}:{line}:5:\\nassertion `left == right` failed\\n  left: [^\\r\\n]+\\n right: [^\\r\\n]+\\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\\n\\n\\nfailures:\\n    {t}\\n\\ntest result: FAILED\\. 0 passed; 1 failed; 0 ignored; 0 measured; 13 filtered out; finished in ([0-9]+\\.[0-9]+)s\\n\\n\\z",
        t = regex::escape(test), p = regex::escape(TEST),
    );
    let Ok(grammar) = regex::Regex::new(&pattern) else {
        return false;
    };
    if !grammar.is_match(out) {
        return false;
    }
    let lines = err.split('\n').collect::<Vec<_>>();
    let [compiling, finished, running, footer, ""] = lines.as_slice() else {
        return false;
    };
    if *compiling != "   Compiling noevia-model-manager v0.1.0 (/Users/sebastiandalton/noevia-work/s1-backend-diagnose-rs/bins/noevia-model-manager)"
        || *footer != "error: test failed, to rerun pass `-p noevia-model-manager --test diagnose`" {
        return false;
    }
    let duration = finished
        .strip_prefix("    Finished `test` profile [unoptimized + debuginfo] target(s) in ")
        .and_then(|s| s.strip_suffix('s'));
    let binary = running
        .strip_prefix(
            "     Running tests/diagnose.rs (/tmp/s1-backend-diagnose-target/debug/deps/diagnose-",
        )
        .and_then(|s| s.strip_suffix(')'));
    duration.is_some_and(decimal)
        && binary.is_some_and(|s| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()))
}
fn replay() -> Result<(), String> {
    if std::env::current_dir().map_err(|e| e.to_string())?
        != Path::new("/Users/sebastiandalton/noevia-work/s1-backend-diagnose-rs")
    {
        return Err("replay checkout identity mismatch".into());
    }
    let raw_certificate = read(&Path::new(EVIDENCE).join("certificate.json"))?;
    if hash(&raw_certificate) != "3a01ed1efae8f40ce8490f25319dea63bcc179fa2e4ea24c77175b424babe189"
    {
        return Err("historical certificate identity mismatch".into());
    }
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&raw_certificate).map_err(|e| e.to_string())?;
    let selected = [
        ("priority", "priority_semantics"),
        ("forced", "forced_semantics"),
        ("slot", "slot_semantics"),
        ("distinct-port", "distinct_port_semantics"),
        ("case", "case_sensitive_success_semantics"),
        ("redaction", "redaction_semantics"),
        ("crossline", "crossline_semantics"),
    ];
    if rows.len() != selected.len() {
        return Err("historical row count".into());
    }
    let source = read(Path::new(SOURCE))?;
    let tests = read(Path::new(TEST))?;
    let fixtures = read(Path::new(
        "bins/noevia-model-manager/tests/fixtures/diagnose-python.v1.json",
    ))?;
    let test_text = std::str::from_utf8(&tests).map_err(|e| e.to_string())?;
    let mut result = Vec::new();
    for (row, (id, test)) in rows.iter().zip(selected) {
        let line = assertion_line(test_text, test)?;
        let out = read(&Path::new(EVIDENCE).join(format!("{id}.stdout")))?;
        let err = read(&Path::new(EVIDENCE).join(format!("{id}.stderr")))?;
        for (key, expected) in [
            ("id", id.to_owned()),
            ("test", test.to_owned()),
            ("assertion_path", TEST.to_owned()),
            ("production_sha256", hash(&source)),
            ("tests_sha256", hash(&tests)),
            ("fixture_sha256", hash(&fixtures)),
            ("stdout_sha256", hash(&out)),
            ("stderr_sha256", hash(&err)),
        ] {
            if row.get(key).and_then(|v| v.as_str()) != Some(expected.as_str()) {
                return Err(format!("historical binding mismatch {id}:{key}"));
            }
        }
        if row.get("assertion_line").and_then(|v| v.as_u64()) != Some(line as u64)
            || row.get("status").and_then(|v| v.as_i64()) != Some(101)
        {
            return Err(format!("historical location/status mismatch {id}"));
        }
        let killed = semantic(
            Some(101),
            std::str::from_utf8(&out).map_err(|e| e.to_string())?,
            std::str::from_utf8(&err).map_err(|e| e.to_string())?,
            test,
            line,
        );
        if !killed {
            return Err(format!("historical transcript rejected {id}"));
        }
        result.push(
            json!({"historical_snapshot":row,"current_classification":killed,"assertion_column":5}),
        );
    }
    println!("{}", serde_json::to_string_pretty(&json!({
        "qualification":"Replay of preserved historical captures; no mutants executed, historical binary hashes not revalidated",
        "historical_certificate_sha256":hash(&raw_certificate),
        "current_checker_sha256":hash(include_bytes!("check-diagnose-mutations.rs")),"rows":result
    })).map_err(|e| e.to_string())?);
    Ok(())
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
    if std::env::args().count() == 2
        && std::env::args().nth(1).as_deref() == Some("--reclassify-preserved")
    {
        replay()
    } else {
        run()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn capture() -> (String, String) {
        ("\nrunning 1 test\ntest priority_semantics ... FAILED\n\nfailures:\n\n---- priority_semantics stdout ----\n\nthread 'priority_semantics' (8027594) panicked at bins/noevia-model-manager/tests/diagnose.rs:105:5:\nassertion `left == right` failed\n  left: String(\"gpu-memory\")\n right: String(\"kv-cache\")\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n\nfailures:\n    priority_semantics\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 13 filtered out; finished in 0.04s\n\n".to_owned(),
         "   Compiling noevia-model-manager v0.1.0 (/Users/sebastiandalton/noevia-work/s1-backend-diagnose-rs/bins/noevia-model-manager)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 1.17s\n     Running tests/diagnose.rs (/tmp/s1-backend-diagnose-target/debug/deps/diagnose-8012b0f2c159e38d)\nerror: test failed, to rerun pass `-p noevia-model-manager --test diagnose`\n".to_owned())
    }
    #[test]
    fn classifier_complete_transcript() {
        let (out, err) = capture();
        assert!(semantic(Some(101), &out, &err, "priority_semantics", 105));
        for text in [
            format!("{out}SETUP\n"),
            format!("SETUP\n{out}"),
            format!("{out}{out}"),
            out.replace(":105:5:", ":105:3:"),
            out.replace("assertion `left == right` failed", "called unwrap on Err"),
            out.replace("13 filtered out", "12 filtered out"),
            out.replace("  left:", "SETUP\n  left:"),
            out.replace("test result: FAILED.", "test result: ok."),
            out.trim_end().to_owned(),
        ] {
            assert!(!semantic(Some(101), &text, &err, "priority_semantics", 105));
        }
        for text in [
            String::new(),
            format!("{err}SETUP\n"),
            format!("SETUP\n{err}"),
            format!("{err}{err}"),
            err.replace("error: test failed", "error: could not compile"),
            err.replace("    Finished", "deadline cap\n    Finished"),
            err.replace(
                "error: test failed, to rerun pass `-p noevia-model-manager --test diagnose`\n",
                "",
            ),
            err.trim_end().to_owned(),
        ] {
            assert!(!semantic(Some(101), &out, &text, "priority_semantics", 105));
        }
        for code in [None, Some(0), Some(1)] {
            assert!(!semantic(code, &out, &err, "priority_semantics", 105));
        }
        assert!(!semantic(Some(101), &out, &err, "other", 105));
        assert!(!semantic(Some(101), &out, &err, "priority_semantics", 106));
    }
}
