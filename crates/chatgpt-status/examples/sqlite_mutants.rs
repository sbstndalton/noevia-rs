#[allow(dead_code)]
#[path = "../../../tools/qualification/mutation.rs"]
mod mutation;
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
fn run() -> Result<(), String> {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mutations = [
        ("src/sqlite.rs", "FROM chatgpt_oauth_tokens WHERE user_id=?1 COLLATE BINARY", "FROM chatgpt_oauth_tokens WHERE ?1 IS NOT NULL", "sqlite", "tenant_and_runtime_caps", "ROUTING_ASSERT/sqlite/tenant"),
        ("src/sqlite.rs", "|| text(i, 4, 256)? != \"BINARY\"", "", "sqlite", "missing_and_incompatible_schema_and_null_pk", "ROUTING_ASSERT/sqlite/schema"),
        ("src/sqlite.rs", "length(CAST(state AS BLOB))<=64", "length(CAST(state AS BLOB))<=65536", "sqlite", "tenant_and_runtime_caps", "ROUTING_ASSERT/sqlite/precopy"),
        ("src/sqlite.rs", "let product = match inner.product_override", "schema(&c, &task.control, true)?; let product = match inner.product_override", "sqlite", "actual_schema_and_product_false_before_oauth", "ROUTING_ASSERT/sqlite/false_before_oauth"),
        ("src/sqlite.rs", "AuthAction::Select | AuthAction::Transaction { .. } => true,", "AuthAction::Select | AuthAction::Update { .. } | AuthAction::Transaction { .. } => true,", "lib", "sqlite::focused::authorizer_and_finite_vm_cancel", "ROUTING_ASSERT/sqlite/authorizer UPDATE settings SET value='false'"),
        ("src/sqlite.rs", "ticks >= 2000", "ticks >= usize::MAX", "lib", "sqlite::focused::authorizer_and_finite_vm_cancel", "ROUTING_ASSERT/sqlite/vm_budget"),
        ("src/sqlite.rs", "if let Some(handle) = h.take() {\n            #[cfg(test)]", "if let Some(handle) = h.take() {\n            drop(lock(&self.lease)?.take());\n            #[cfg(test)]", "lib", "sqlite::focused::global_eight_owner_one_and_join_retains_lease", "ROUTING_ASSERT/sqlite/retained_lease"),
    ];
    let start = std::env::args()
        .nth(1)
        .map(|s| s.parse::<usize>())
        .transpose()
        .map_err(|_| "invalid mutant start")?
        .unwrap_or(0);
    if start > mutations.len() {
        return Err("invalid mutant start".into());
    }
    for (file, old, new, suite, test, id) in mutations.into_iter().skip(start) {
        let path = base.join(file);
        let bytes = std::fs::read(&path).map_err(|_| "mutant source read")?;
        let source = std::str::from_utf8(&bytes).map_err(|_| "mutant source UTF8")?;
        if source.matches(old).count() != 1 {
            return Err(format!("mutant target not unique: {test}"));
        }
        let mut changed = source.replacen(old, new, 1);
        if id == "ROUTING_ASSERT/sqlite/precopy" {
            let guard = "let state = text(r, 1, 64)?;";
            if changed.matches(guard).count() != 1 {
                return Err("state guard target not unique".into());
            }
            changed = changed.replacen(guard, "let state = text(r, 1, 65536)?;", 1);
        }
        let restore = Restore {
            path: path.clone(),
            bytes,
        };
        std::fs::write(&path, changed).map_err(|_| "mutant source write")?;
        let mut c = Command::new(
            "/Users/sebastiandalton/.rustup/toolchains/1.99.0-aarch64-apple-darwin/bin/cargo",
        );
        c.args(["test", "--offline", "--manifest-path"])
            .arg(base.join("Cargo.toml"));
        c.args([
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
        let classified = mutation::classify(result);
        std::fs::write(&path, &restore.bytes).map_err(|_| "explicit mutant restore")?;
        if std::fs::read(&path).map_err(|_| "mutant restore read")? != restore.bytes {
            return Err("mutant restore mismatch".into());
        }
        mutation::reject_mutant(test, id, classified)?;
        drop(restore);
        println!("semantic mutant rejected: {test}");
    }
    Ok(())
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
