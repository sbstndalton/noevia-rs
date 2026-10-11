//! Private closed-fixture qualification; no production helper/DNS execution.
#[allow(dead_code)]
#[path = "../../../../tools/qualification/process.rs"]
mod process;
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::Path, process::Command, time::Duration};
const SOURCE: &str = "bins/noevia-server/src/code_net_resolver.rs";
const TEST: &str = "bins/noevia-server/tests/support/resolver_controls.rs";
const PANIC_PATH: &str = "bins/noevia-server/src/../tests/support/resolver_controls.rs";
const PACKET: &str = "/tmp/m6-resolver-wave-a-mutants";
fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
fn test_binary_hash(stderr: &str) -> Result<String, String> {
    let prefix = "     Running unittests src/lib.rs (";
    let mut paths = stderr
        .lines()
        .filter_map(|s| s.strip_prefix(prefix).and_then(|s| s.strip_suffix(')')));
    let path = paths.next().ok_or("binary absent")?;
    if paths.next().is_some() {
        return Err("multiple binaries".into());
    }
    let suffix = path
        .strip_prefix("/tmp/m6-resolver-wave-a-target/debug/deps/noevia_server-")
        .ok_or("binary target")?;
    if suffix.len() != 16 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("binary identity".into());
    }
    let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
    if file.metadata().map_err(|e| e.to_string())?.len() > 128 * 1024 * 1024 {
        return Err("binary cap".into());
    }
    let mut digest = Sha256::new();
    let mut bytes = [0u8; 65536];
    let mut total = 0usize;
    loop {
        let n = file.read(&mut bytes).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        total = total
            .checked_add(n)
            .filter(|n| *n <= 128 * 1024 * 1024)
            .ok_or("binary cap")?;
        digest.update(bytes.get(..n).ok_or("binary read")?);
    }
    Ok(format!("{:x}", digest.finalize()))
}
fn read(p: &str) -> Result<Vec<u8>, String> {
    let f = fs::File::open(p).map_err(|e| e.to_string())?;
    if f.metadata().map_err(|e| e.to_string())?.len() > 4 * 1024 * 1024 {
        return Err("cap".into());
    }
    let mut b = Vec::new();
    f.take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut b)
        .map_err(|e| e.to_string())?;
    if b.len() > 4 * 1024 * 1024 {
        return Err("cap".into());
    }
    Ok(b)
}
fn line(source: &str, test: &str) -> Result<usize, String> {
    let start = source.find(&format!("fn {test}(")).ok_or("test absent")?;
    let region = source.get(start..).ok_or("region")?;
    let region = region
        .get(..region.find("\n}\n").ok_or("end")?)
        .ok_or("region end")?;
    if region.matches("assert_eq!").count() != 1 {
        return Err("nonunique assertion".into());
    }
    let offset = start + region.find("assert_eq!").ok_or("assert absent")?;
    let prefix = source.get(..offset).ok_or("prefix")?;
    if prefix.rsplit('\n').next() != Some("    ") {
        return Err("column".into());
    }
    Ok(prefix.bytes().filter(|b| *b == b'\n').count() + 1)
}
fn semantic(
    code: Option<i32>,
    out: &str,
    err: &str,
    test: &str,
    line: usize,
    filtered: usize,
) -> bool {
    if code != Some(101) {
        return false;
    }
    let name = format!("code_net_resolver::tests::{test}");
    let mut it = out.split('\n');
    for value in [
        "".to_owned(),
        "running 1 test".into(),
        format!("test {name} ... FAILED"),
        "".into(),
        "failures:".into(),
        "".into(),
        format!("---- {name} stdout ----"),
        "".into(),
    ] {
        if it.next() != Some(value.as_str()) {
            return false;
        }
    }
    let Some(panic) = it.next() else { return false };
    let Some(tail) = panic.strip_prefix(&format!("thread '{name}' (")) else {
        return false;
    };
    let Some((pid, loc)) = tail.split_once(") panicked at ") else {
        return false;
    };
    if pid.is_empty()
        || pid.len() > 20
        || !pid.bytes().all(|b| b.is_ascii_digit())
        || loc != format!("{PANIC_PATH}:{line}:5:")
    {
        return false;
    }
    if it.next() != Some("assertion `left == right` failed") {
        return false;
    }
    for prefix in ["  left: ", " right: "] {
        if !it.next().is_some_and(|s| {
            s.strip_prefix(prefix)
                .is_some_and(|v| !v.is_empty() && !v.contains('\r'))
        }) {
            return false;
        }
    }
    for value in [
        "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace".to_owned(),
        "".into(),
        "".into(),
        "failures:".into(),
        format!("    {name}"),
        "".into(),
    ] {
        if it.next() != Some(value.as_str()) {
            return false;
        }
    }
    let Some(summary) = it.next() else {
        return false;
    };
    let Some(timing)=summary.strip_prefix(&format!("test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; {filtered} filtered out; finished in ")).and_then(|s|s.strip_suffix('s')) else{return false};
    if !decimal(timing) || it.next() != Some("") || it.next() != Some("") || it.next().is_some() {
        return false;
    }
    let e = err.split('\n').collect::<Vec<_>>();
    let [compiling, finished, running, footer, ""] = e.as_slice() else {
        return false;
    };
    *compiling=="   Compiling noevia-server v0.1.0 (/Users/sebastiandalton/noevia-work/m6-chatgpt-status-private/bins/noevia-server)"
 &&finished.strip_prefix("    Finished `test` profile [unoptimized + debuginfo] target(s) in ").and_then(|s|s.strip_suffix('s')).is_some_and(decimal)
 &&running.strip_prefix("     Running unittests src/lib.rs (/tmp/m6-resolver-wave-a-target/debug/deps/noevia_server-").and_then(|s|s.strip_suffix(')')).is_some_and(|s|s.len()==16&&s.bytes().all(|b|b.is_ascii_hexdigit()))
 &&*footer=="error: test failed, to rerun pass `-p noevia-server --lib`"
}
fn decimal(s: &str) -> bool {
    s.split_once('.').is_some_and(|(a, b)| {
        !a.is_empty() && !b.is_empty() && a.bytes().chain(b.bytes()).all(|c| c.is_ascii_digit())
    })
}
struct Restore(Vec<u8>);
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = fs::write(SOURCE, &self.0);
    }
}
fn replay() -> Result<(), String> {
    let original = read(&format!("{PACKET}/original-source.rs"))?;
    let tests = read(&format!("{PACKET}/assertion-source.rs"))?;
    let test_text = std::str::from_utf8(&tests).map_err(|e| e.to_string())?;
    let raw = read(&format!("{PACKET}/certificate.json"))?;
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
    let expected = [
        ("stale", "selected_stale_generation_assertion"),
        ("union", "selected_lifetime_union_assertion"),
        ("expiry", "selected_expiry_assertion"),
        ("global", "selected_global_configuration_assertion"),
        ("reap", "selected_reap_guard_assertion"),
    ];
    if rows.len() != expected.len() {
        return Err("inventory".into());
    }
    let mut result = Vec::new();
    for (row, (id, test)) in rows.iter().zip(expected) {
        let stdout = read(&format!("{PACKET}/{id}.stdout"))?;
        let stderr = read(&format!("{PACKET}/{id}.stderr"))?;
        let mutant = read(&format!("{PACKET}/{id}.source.rs"))?;
        for (key, value) in [
            ("id", id.to_owned()),
            ("test", test.to_owned()),
            ("source_sha256", hash(&original)),
            ("tests_sha256", hash(&tests)),
            ("mutant_sha256", hash(&mutant)),
            ("stdout_sha256", hash(&stdout)),
            ("stderr_sha256", hash(&stderr)),
        ] {
            if row.get(key).and_then(|v| v.as_str()) != Some(value.as_str()) {
                return Err(format!("binding {id}:{key}"));
            }
        }
        let anchor = line(test_text, test)?;
        if row.get("line").and_then(|v| v.as_u64()) != Some(anchor as u64)
            || row.get("column").and_then(|v| v.as_u64()) != Some(5)
            || row.get("status").and_then(|v| v.as_i64()) != Some(101)
        {
            return Err("location/status".into());
        }
        let out = std::str::from_utf8(&stdout).map_err(|e| e.to_string())?;
        let err = std::str::from_utf8(&stderr).map_err(|e| e.to_string())?;
        let killed = semantic(
            Some(101),
            out,
            err,
            test,
            anchor,
            test_text.matches("#[test]").count() + 44 - 1,
        );
        if !killed {
            return Err(format!("transcript {id}"));
        }
        result.push(serde_json::json!({"historical_snapshot":row,"current_classification":killed}));
    }
    println!("{}",serde_json::to_string_pretty(&serde_json::json!({"qualification":"historical capture replay only; no mutants executed; historical binary hashes not revalidated","historical_certificate_sha256":hash(&raw),"current_checker_sha256":hash(include_bytes!("check-resolver-mutations.rs")),"rows":result})).map_err(|e|e.to_string())?);
    Ok(())
}
fn main() -> Result<(), String> {
    if std::env::args().count() == 2
        && std::env::args().nth(1).as_deref() == Some("--reclassify-preserved")
    {
        return replay();
    }

    if std::env::current_dir().map_err(|e| e.to_string())?
        != Path::new("/Users/sebastiandalton/noevia-work/m6-chatgpt-status-private")
    {
        return Err("checkout".into());
    }
    if std::env::args().count() != 2 || std::env::args().nth(1).as_deref() != Some("--run-once") {
        return Err("--run-once required".into());
    }
    fs::create_dir(PACKET).map_err(|e| e.to_string())?;
    let original = read(SOURCE)?;
    let source = std::str::from_utf8(&original).map_err(|e| e.to_string())?;
    let tests = read(TEST)?;
    let test_text = std::str::from_utf8(&tests).map_err(|e| e.to_string())?;
    let selected=[("stale","c.generation()? != generation","c.generation()? == generation","selected_stale_generation_assertion"),
 ("union","for addr in found {","union.clear(); for addr in found {","selected_lifetime_union_assertion"),
 ("expiry","now >= t","now > t","selected_expiry_assertion"),
 ("global","if owner.spec != spec {","if false && owner.spec != spec {","selected_global_configuration_assertion"),
 ("reap","if self.owner.lease.load(Ordering::Acquire) || self.owner.quarantine.load(Ordering::Acquire)","if false || self.owner.quarantine.load(Ordering::Acquire)","selected_reap_guard_assertion")];
    let mut rows = Vec::new();
    for (id, before, after, test) in selected {
        if source.matches(before).count() != 1 {
            return Err(format!("nonunique {id}"));
        }
        let mutant = source.replace(before, after);
        let restore = Restore(original.clone());
        fs::write(SOURCE, &mutant).map_err(|e| e.to_string())?;
        let mut command = Command::new(
            "/Users/sebastiandalton/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin/cargo",
        );
        command.env_clear().env("PATH","/Users/sebastiandalton/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin:/usr/bin:/bin").env("HOME","/Users/sebastiandalton").env("CARGO_HOME","/Users/sebastiandalton/.cargo").env("CARGO_TARGET_DIR","/tmp/m6-resolver-wave-a-target").args(["test","--offline","--locked","-p","noevia-server","--lib",&format!("code_net_resolver::tests::{test}"),"--","--exact","--test-threads=1"]);
        let result = process::bounded(command, Vec::new(), Duration::from_secs(60));
        fs::write(SOURCE, &restore.0).map_err(|e| e.to_string())?;
        drop(restore);
        let o = result?;
        fs::write(format!("{PACKET}/{id}.source.rs"), &mutant).map_err(|e| e.to_string())?;
        fs::write(format!("{PACKET}/{id}.stdout"), &o.stdout).map_err(|e| e.to_string())?;
        fs::write(format!("{PACKET}/{id}.stderr"), &o.stderr).map_err(|e| e.to_string())?;
        fs::write(
            format!("{PACKET}/{id}.status"),
            format!("{:?}\n", o.status.code()),
        )
        .map_err(|e| e.to_string())?;
        let anchor = line(test_text, test)?;
        let stdout = std::str::from_utf8(&o.stdout).map_err(|e| e.to_string())?;
        let stderr = std::str::from_utf8(&o.stderr).map_err(|e| e.to_string())?;
        let killed = semantic(
            o.status.code(),
            stdout,
            stderr,
            test,
            anchor,
            test_text.matches("#[test]").count() + 44 - 1,
        );
        let row = serde_json::json!({"id":id,"test":test,"line":anchor,"column":5,"filtered":test_text.matches("#[test]").count()+44-1,"status":o.status.code(),"semantic_kill":killed,"source_sha256":hash(&original),"mutant_sha256":hash(mutant.as_bytes()),"tests_sha256":hash(&tests),"fixture_binary_sha256":hash(&read("/tmp/m6-resolver-fixture-v1")?),"stdout_sha256":hash(&o.stdout),"stderr_sha256":hash(&o.stderr),"test_binary_sha256":test_binary_hash(stderr)?,"checker_sha256":hash(include_bytes!("check-resolver-mutations.rs"))});
        rows.push(row);
        if !killed {
            fs::write(
                format!("{PACKET}/partial-certificate.json"),
                serde_json::to_vec_pretty(&rows).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            return Err(format!("{id} not classified; retain raw, do not rerun"));
        }
        for (out, err, code) in [
            (format!("{stdout}SETUP\n"), stderr.to_owned(), Some(101)),
            (stdout.to_owned(), format!("{stderr}SETUP\n"), Some(101)),
            (
                stdout.replace(&format!(":{anchor}:5:"), &format!(":{anchor}:3:")),
                stderr.to_owned(),
                Some(101),
            ),
            (stdout.to_owned(), String::new(), Some(101)),
            (format!("{stdout}{stdout}"), stderr.to_owned(), Some(101)),
            (stdout.to_owned(), stderr.to_owned(), Some(0)),
        ] {
            if semantic(
                code,
                &out,
                &err,
                test,
                anchor,
                test_text.matches("#[test]").count() + 44 - 1,
            ) {
                return Err("negative classification accepted".into());
            }
        }
        println!("{id}: exact semantic assertion killed once; six forged controls rejected");
    }
    fs::write(
        format!("{PACKET}/certificate.json"),
        serde_json::to_vec_pretty(&rows).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
