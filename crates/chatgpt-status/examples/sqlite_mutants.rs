#[allow(dead_code)]
#[path = "support/sqlite_assertions.rs"]
mod assertions;
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
use std::{
    path::PathBuf,
    process::{Command, ExitCode},
    time::Duration,
};
struct Restore {
    path: PathBuf,
    bytes: Vec<u8>,
}
impl Drop for Restore {
    fn drop(&mut self) {
        if let Err(error) = std::fs::write(&self.path, &self.bytes) {
            eprintln!("MUTATION RESTORE FAILED: {error}")
        }
    }
}
fn matrix() -> [(
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
); 7] {
    [
        ("src/sqlite.rs", "FROM chatgpt_oauth_tokens WHERE user_id=?1 COLLATE BINARY", "FROM chatgpt_oauth_tokens WHERE ?1 IS NOT NULL", "sqlite", "tenant_and_runtime_caps", "ROUTING_ASSERT/sqlite/tenant"),
        ("src/sqlite.rs", "|| text(i, 4, 256)? != \"BINARY\"", "", "sqlite", "missing_and_incompatible_schema_and_null_pk", "ROUTING_ASSERT/sqlite/schema"),
        ("src/sqlite.rs", "length(CAST(state AS BLOB))<=64", "length(CAST(state AS BLOB))<=65536", "sqlite", "tenant_and_runtime_caps", "ROUTING_ASSERT/sqlite/precopy"),
        ("src/sqlite.rs", "let product = match inner.product_override", "schema(&c, &task.control, true)?; let product = match inner.product_override", "sqlite", "actual_schema_and_product_false_before_oauth", "ROUTING_ASSERT/sqlite/false_before_oauth"),
        ("src/sqlite.rs", "AuthAction::Select | AuthAction::Transaction { .. } => true,", "AuthAction::Select | AuthAction::Update { .. } | AuthAction::Transaction { .. } => true,", "lib", "sqlite::focused::authorizer_and_finite_vm_cancel", "ROUTING_ASSERT/sqlite/authorizer UPDATE settings SET value='false'"),
        ("src/sqlite.rs", "ticks >= 2000", "ticks >= usize::MAX", "lib", "sqlite::focused::authorizer_and_finite_vm_cancel", "ROUTING_ASSERT/sqlite/vm_budget"),
        ("src/sqlite.rs", "if let Some(handle) = h.take() {\n            #[cfg(test)]", "if let Some(handle) = h.take() {\n            drop(lock(&self.lease)?.take());\n            #[cfg(test)]", "lib", "sqlite::focused::global_eight_owner_one_and_join_retains_lease", "ROUTING_ASSERT/sqlite/retained_lease"),
    ]
}
const TAGS: [&str; 7] = [
    "tenant_predicate",
    "binary_pk",
    "precopy_guards",
    "product_boundary",
    "writer_authorizer",
    "vm_counter",
    "retained_join_lease",
];
fn anchors(index: usize) -> Vec<assertions::Anchor> {
    use assertions::{Anchor, Kind};
    match index {
        0 => vec![Anchor {
            statement: r#"assert_eq!(
        f.read(Some("true"), "u2")
            .map(|a| a.1.contains("disconnected")),
        Ok(true),
        "ROUTING_ASSERT/sqlite/tenant"
    );"#,
            payload: "ROUTING_ASSERT/sqlite/tenant",
            kind: Kind::Equal,
        }],
        1 => vec![Anchor {
            statement: r#"assert_eq!(f.read(Some("true"),"u"),Err(Error::Unavailable),"ROUTING_ASSERT/sqlite/schema");"#,
            payload: "ROUTING_ASSERT/sqlite/schema",
            kind: Kind::Equal,
        }],
        2 => vec![Anchor {
            statement: r#"assert_eq!(
            f.read(Some("true"), "u1"),
            Err(Error::Unavailable),
            "ROUTING_ASSERT/sqlite/precopy"
        );"#,
            payload: "ROUTING_ASSERT/sqlite/precopy",
            kind: Kind::Equal,
        }],
        3 => vec![Anchor {
            statement: r#"assert_eq!(
        f.read(None, "u1").map(|a| a.0),
        Ok(404),
        "ROUTING_ASSERT/sqlite/false_before_oauth"
    );"#,
            payload: "ROUTING_ASSERT/sqlite/false_before_oauth",
            kind: Kind::Equal,
        }],
        4 => vec![Anchor {
            statement: r#"assert!(
                c.prepare(statement).is_err(),
                "ROUTING_ASSERT/sqlite/authorizer {statement}"
            );"#,
            payload: "ROUTING_ASSERT/sqlite/authorizer UPDATE settings SET value='false'",
            kind: Kind::Boolean,
        }],
        5 => vec![
            Anchor {
                statement: r#"assert_eq!(
            result.unwrap_err().sqlite_error_code(),
            Some(rusqlite::ErrorCode::OperationInterrupted),
            "ROUTING_ASSERT/sqlite/vm_budget"
        );"#,
                payload: "ROUTING_ASSERT/sqlite/vm_budget",
                kind: Kind::Equal,
            },
            Anchor {
                statement: r#"assert!(
            start.elapsed() < Duration::from_secs(1),
            "ROUTING_ASSERT/sqlite/vm_budget"
        );"#,
                payload: "ROUTING_ASSERT/sqlite/vm_budget",
                kind: Kind::Boolean,
            },
        ],
        6 => vec![Anchor {
            statement: r#"assert!(
            GlobalLease::acquire(Some("retained-unit")).is_err(),
            "ROUTING_ASSERT/sqlite/retained_lease"
        );"#,
            payload: "ROUTING_ASSERT/sqlite/retained_lease",
            kind: Kind::Boolean,
        }],
        _ => Vec::new(),
    }
}
fn capped(path: &std::path::Path, cap: usize) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| "evidence read")?
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "evidence read")?;
    if bytes.len() > cap {
        return Err("evidence/source cap".into());
    }
    Ok(bytes)
}
fn changed(source: &str, old: &str, new: &str, id: &str) -> Result<String, String> {
    if source.matches(old).count() != 1 {
        return Err("nonunique mutation guard".into());
    }
    let mut source = source.replacen(old, new, 1);
    if id == "ROUTING_ASSERT/sqlite/precopy" {
        let guard = "let state = text(r, 1, 64)?;";
        if source.matches(guard).count() != 1 {
            return Err("nonunique second precopy guard".into());
        }
        source = source.replacen(guard, "let state = text(r, 1, 65536)?;", 1);
    }
    Ok(source)
}
fn verify(base: &std::path::Path, dir: &std::path::Path) -> Result<(), String> {
    let mut certificates = Vec::new();
    for (index, (file, old, new, suite, test, id)) in matrix().into_iter().enumerate() {
        let tag = *TAGS.get(index).ok_or("inventory tag")?;
        let original = capped(&base.join(file), 1048576)?;
        let expected = changed(
            std::str::from_utf8(&original).map_err(|_| "source UTF8")?,
            old,
            new,
            id,
        )?;
        if capped(&dir.join(format!("{tag}.source.rs")), 1048576)? != expected.as_bytes() {
            return Err("current mutation source binding mismatch".into());
        }
        let assertion_path = if suite == "lib" {
            "src/sqlite.rs"
        } else {
            "tests/sqlite.rs"
        };
        let assertion_source = if suite == "lib" {
            expected.clone()
        } else {
            String::from_utf8(capped(&base.join(assertion_path), 1048576)?)
                .map_err(|_| "assertion UTF8")?
        };
        if capped(&dir.join(format!("{tag}.assertions.rs")), 1048576)?
            != assertion_source.as_bytes()
        {
            return Err("current assertion source binding mismatch".into());
        }
        let binding = assertions::bind(&assertion_source, assertion_path, test, &anchors(index))?;
        let stdout = capped(&dir.join(format!("{tag}.stdout")), 8 * 1024 * 1024)?;
        let stderr = capped(&dir.join(format!("{tag}.stderr")), 64 * 1024)?;
        let meta: serde_json::Value =
            serde_json::from_slice(&capped(&dir.join(format!("{tag}.json")), 65536)?)
                .map_err(|_| "capture metadata JSON")?;
        if meta.get("original_source_sha256")
            != Some(&serde_json::json!(assertions::hash(&original)))
            || meta.get("assertion_source_sha256")
                != Some(&serde_json::json!(assertions::hash(
                    assertion_source.as_bytes()
                )))
            || meta.get("stdout_bytes") != Some(&serde_json::json!(stdout.len()))
            || meta.get("stderr_bytes") != Some(&serde_json::json!(stderr.len()))
            || meta.get("tag") != Some(&serde_json::json!(tag))
            || meta.get("index") != Some(&serde_json::json!(index))
            || meta.get("source_sha256")
                != Some(&serde_json::json!(assertions::hash(expected.as_bytes())))
            || meta.get("stdout_sha256") != Some(&serde_json::json!(assertions::hash(&stdout)))
            || meta.get("stderr_sha256") != Some(&serde_json::json!(assertions::hash(&stderr)))
            || meta.get("test") != Some(&serde_json::json!(test))
            || meta.get("policy") != Some(&serde_json::json!(id))
            || !meta
                .get("process_error")
                .is_some_and(serde_json::Value::is_null)
        {
            return Err("capture inventory/hash/process mismatch".into());
        }
        let exit = meta
            .get("exit")
            .and_then(serde_json::Value::as_i64)
            .and_then(|n| i32::try_from(n).ok());
        let witness = assertions::certify(exit, &stdout, &stderr, &binding, &assertion_source)?;
        certificates.push(serde_json::json!({"index":index,"tag":tag,"binding":binding,"witness":witness,"stdout_sha256":assertions::hash(&stdout),"stderr_sha256":assertions::hash(&stderr),"source_sha256":assertions::hash(expected.as_bytes())}));
    }
    if certificates.len() != 7 {
        return Err("incomplete final mutant inventory".into());
    }
    let certificate = serde_json::json!({"inventory":TAGS,"count":7,"classifier_sha256":assertions::hash(include_bytes!("support/sqlite_assertions.rs")),"runner_sha256":assertions::hash(include_bytes!("sqlite_mutants.rs")),"cases":certificates});
    std::fs::write(
        dir.join("certificate.json"),
        serde_json::to_vec_pretty(&certificate).map_err(|_| "certificate encode")?,
    )
    .map_err(|_| "certificate write")?;
    println!("exact current-source mutant inventory certified: 7/7");
    Ok(())
}
fn run() -> Result<(), String> {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut args = std::env::args().skip(1);
    let first = args.next().ok_or("fresh evidence directory required")?;
    let replay = first == "--certify";
    let dir = PathBuf::from(if replay {
        args.next().ok_or("certificate directory required")?
    } else {
        first
    });
    if args.next().is_some() || !dir.is_absolute() || dir.as_os_str().len() > 4096 {
        return Err("invalid evidence arguments".into());
    }
    if replay {
        return verify(&base, &dir);
    }
    std::fs::create_dir(&dir).map_err(|_| "fresh evidence directory required")?;
    for (index, (file, old, new, suite, test, id)) in matrix().into_iter().enumerate() {
        let tag = *TAGS.get(index).ok_or("inventory tag")?;
        let path = base.join(file);
        let bytes = capped(&path, 1048576)?;
        let source = String::from_utf8(bytes.clone()).map_err(|_| "source UTF8")?;
        let mutation = changed(&source, old, new, id)?;
        let assertion_source = if suite == "lib" {
            mutation.clone()
        } else {
            String::from_utf8(capped(&base.join("tests/sqlite.rs"), 1048576)?)
                .map_err(|_| "assertion UTF8")?
        };
        // Bind before executing; missing/ambiguous anchors refuse to run the mutant.
        assertions::bind(
            &assertion_source,
            if suite == "lib" {
                "src/sqlite.rs"
            } else {
                "tests/sqlite.rs"
            },
            test,
            &anchors(index),
        )?;
        std::fs::write(dir.join(format!("{tag}.source.rs")), &mutation)
            .map_err(|_| "source evidence write")?;
        std::fs::write(dir.join(format!("{tag}.assertions.rs")), &assertion_source)
            .map_err(|_| "assertion evidence write")?;
        let restore = Restore {
            path: path.clone(),
            bytes,
        };
        std::fs::write(&path, &mutation).map_err(|_| "mutation write")?;
        let mut c = Command::new(
            "/Users/sebastiandalton/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin/cargo",
        );
        c.args(["test", "--offline", "--manifest-path"])
            .arg(base.join("Cargo.toml"))
            .args([
                "--target-dir",
                "/tmp/m6-chatgpt-status-sqlite-mutant-target",
            ]);
        if suite == "lib" {
            c.arg("--lib");
        } else {
            c.args(["--test", suite]);
        }
        c.args([test, "--", "--exact", "--nocapture", "--test-threads=1"]);
        c.env(
            "RUSTC",
            "/Users/sebastiandalton/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin/rustc",
        );
        let result = process::bounded(c, Vec::new(), Duration::from_secs(45));
        let current = capped(&path, 1048576)?;
        std::fs::write(&path, &restore.bytes).map_err(|_| "restore write")?;
        if current != mutation.as_bytes() || capped(&path, 1048576)? != restore.bytes {
            return Err("source/restoration byte mismatch".into());
        }
        drop(restore);
        let (exit, stdout, stderr, error) = match result {
            Ok(o) => (o.status.code(), o.stdout, o.stderr, None),
            Err(e) => (None, Vec::new(), Vec::new(), Some(e)),
        };
        std::fs::write(dir.join(format!("{tag}.stdout")), &stdout).map_err(|_| "stdout write")?;
        std::fs::write(dir.join(format!("{tag}.stderr")), &stderr).map_err(|_| "stderr write")?;
        let meta = serde_json::json!({"index":index,"tag":tag,"test":test,"policy":id,"exit":exit,"process_error":error,"source_sha256":assertions::hash(mutation.as_bytes()),"original_source_sha256":assertions::hash(source.as_bytes()),"assertion_source_sha256":assertions::hash(assertion_source.as_bytes()),"stdout_sha256":assertions::hash(&stdout),"stderr_sha256":assertions::hash(&stderr),"stdout_bytes":stdout.len(),"stderr_bytes":stderr.len()});
        std::fs::write(
            dir.join(format!("{tag}.json")),
            serde_json::to_vec_pretty(&meta).map_err(|_| "meta encode")?,
        )
        .map_err(|_| "meta write")?;
        println!("bounded mutant captured/restored: {tag}");
    }
    verify(&base, &dir)
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mutation qualification failed: {e}");
            ExitCode::FAILURE
        }
    }
}
