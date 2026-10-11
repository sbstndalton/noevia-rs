//! Controlled production mutations; semantic test failure is required, compilation failure cannot pass.
use std::{
    path::PathBuf,
    process::{Command, ExitCode},
};
#[path = "../../../tools/qualification/mutation.rs"]
mod mutation;
#[allow(dead_code)]
#[path = "../../../tools/qualification/process.rs"]
mod process;
struct Restore {
    path: PathBuf,
    source: String,
}
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.path, &self.source);
    }
}
fn run() -> Result<(), String> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let out = PathBuf::from(
        std::env::args()
            .nth(1)
            .ok_or("new output directory required")?,
    );
    if out.exists() {
        return Err("mutation output must be new".into());
    }
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    for (name, file, old, new, package, test, filter) in [
        (
            "last-duplicate",
            "crates/server-routing-mode/src/input.rs",
            "out = Some(Self(value.into()));",
            "if out.is_none() { out = Some(Self(value.into())); }",
            "server-routing-mode",
            "differential",
            "actual_node_configuration",
        ),
        (
            "js-bom-trim",
            "crates/server-routing-mode/src/lib.rs",
            "|'\\u{feff}'",
            "",
            "server-routing-mode",
            "differential",
            "actual_node_configuration",
        ),
        (
            "cloud-model-fallback",
            "crates/server-routing-mode/src/lib.rs",
            "next.models.iter().all(String::is_empty)",
            "next.models.get(1).is_none_or(String::is_empty)",
            "server-routing-mode",
            "differential",
            "actual_node_configuration",
        ),
        (
            "stored-disallowed",
            "crates/server-routing-mode/src/lib.rs",
            "} else {\n                \"local\"\n            }",
            "} else { m.as_str() }",
            "server-routing-mode",
            "differential",
            "actual_node_configuration",
        ),
        (
            "allowed-persistence",
            "crates/server-routing-mode/src/lib.rs",
            "writer.write(|tx| tx.set_routing_allowed(cap, &text))?;",
            "let _ = (&cap, &text);",
            "server-routing-mode",
            "differential",
            "actual_node_configuration",
        ),
        (
            "audit-actor",
            "crates/server-routing-mode/src/lib.rs",
            "VALUES(?1,?1,'routing.allowed',?2,?3)",
            "VALUES(?1,NULL,'routing.allowed',?2,?3)",
            "server-routing-mode",
            "differential",
            "actual_node_configuration",
        ),
        (
            "file-no-follow",
            "crates/server-routing-mode/src/storage.rs",
            "OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC",
            "OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC",
            "server-routing-mode",
            "storage",
            "tenant_boundaries",
        ),
        (
            "allowed-member",
            "bins/noevia-server/src/routing_mode_routes.rs",
            "if allowed_path && !admin {",
            "if false && allowed_path && !admin {",
            "noevia-server",
            "routing_mode",
            "native_configuration",
        ),
    ] {
        let path = root.join(file);
        let source = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        if source.matches(old).count() != 1 {
            return Err(format!("mutation {name} needs exactly one source target"));
        }
        let restore = Restore {
            path: path.clone(),
            source: source.clone(),
        };
        std::fs::write(&path, source.replacen(old, new, 1)).map_err(|e| e.to_string())?;
        let mut c = Command::new("cargo");
        c.current_dir(&root)
            .env("CARGO_TARGET_DIR", out.join("target"))
            .args(["test", "--locked", "-p", package, "--test", test, filter]);
        if package == "noevia-server" {
            c.args(["--features", "rust-routing-mode"]);
        }
        let result = process::bounded(c, Vec::new(), std::time::Duration::from_secs(600));
        drop(restore);
        let result = result.map_err(|e| format!("infrastructure: {name}: {e}"))?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        std::fs::write(out.join(format!("{name}.log")), &text).map_err(|e| e.to_string())?;
        let matrix = "actual_node_configuration_file_sql_and_audit_matrix";
        let (expected_test, expected_id) = match name {
            "last-duplicate" => (matrix, "ROUTING_ASSERT/body/account/23/None"),
            "js-bom-trim" => (matrix, "ROUTING_ASSERT/status/account/47/None"),
            "cloud-model-fallback" => (matrix, "ROUTING_ASSERT/status/account/27/None"),
            "stored-disallowed" => (matrix, "ROUTING_ASSERT/body/get/Some(\"{\\\"mode\\\":\\\"hybrid\\\",\\\"whenSensitive\\\":\\\"bad\\\",\\\"cloud\\\":null}\")/Some(\"[]\")"),
            "allowed-persistence" => (matrix, "ROUTING_ASSERT/allowed/allowed/[]/admin"),
            "audit-actor" => (matrix, "ROUTING_ASSERT/audit/allowed/[]/admin"),
            "file-no-follow" => ("tenant_boundaries_regular_files_atomic_bytes_and_cancellation", "ROUTING_ASSERT/file-symlink-refused"),
            "allowed-member" => ("native_configuration_identity_device_csrf_tenants_dynamic_feature_and_failures", "ROUTING_ASSERT/allowed-admin-required"),
            _ => return Err("missing mutant assertion identity".into()),
        };
        mutation::reject_mutant(expected_test, expected_id, mutation::classify(Ok(result)))?;
        println!("production mutant {name} detected");
    }
    Ok(())
}
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("routing mutation qualification failed: {e}");
            ExitCode::FAILURE
        }
    }
}
