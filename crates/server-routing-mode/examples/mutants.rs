//! Controlled production mutations; semantic test failure is required, compilation failure cannot pass.
use std::{
    path::PathBuf,
    process::{Command, ExitCode},
};
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
            .args(["test", "--locked", "-p", package, "--test", test, filter]);
        if package == "noevia-server" {
            c.args(["--features", "rust-routing-mode"]);
        }
        let result = c.output().map_err(|e| e.to_string());
        drop(restore);
        let result = result?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        std::fs::write(out.join(format!("{name}.log")), &text).map_err(|e| e.to_string())?;
        if result.status.success() || !text.contains("FAILED") || text.contains("could not compile")
        {
            return Err(format!(
                "production mutant {name} lacked a semantic test refusal"
            ));
        }
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
