//! Isolated-checkout assertion controls; restores exact bytes before examining results.
#![forbid(unsafe_code)]
use std::{fs, path::Path, process::Command};
fn mutation(
    path: &Path,
    before: &str,
    after: &str,
    package: &str,
    filter: &str,
    limits: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let original = fs::read_to_string(path)?;
    if original.matches(before).count() != 1 {
        return Err(format!("nonunique target: {filter}").into());
    }
    fs::write(path, original.replace(before, after))?;
    let mut command = Command::new("cargo");
    command.args(["test", "--locked", "-q", "-p", package]);
    if matches!(
        filter,
        "capability_closed_stdout_returns_error_without_panic_or_child"
            | "child_port_guard_excludes_second_fixed_port_owner"
    ) {
        command.args([
            "--features",
            "native-host,native-sections",
            "--test",
            "lifecycle",
        ]);
    } else if limits {
        command.args(["--test", "limits"]);
    } else {
        command.args(["--features", "native-host,native-sections", "--lib"]);
    }
    let outcome = command.arg(filter).output();
    fs::write(path, original)?;
    let outcome = outcome?;
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    if outcome.status.success()
        || !stdout.contains("test result: FAILED")
        || !stdout.contains(filter)
    {
        return Err(format!("mutation did not reach failing assertion: {filter}").into());
    }
    println!("detected {filter}: {before}");
    Ok(())
}
fn auth_mutation() -> Result<(), Box<dyn std::error::Error>> {
    mutation(
        Path::new("bins/noevia-model-manager/src/lib.rs"),
        "if auth_path != \"/api/v1/health\" && !state.token.is_empty()",
        "if auth_path != \"/api/v1/health\" && auth_path != \"/api/v1/sections/tiny\" && !state.token.is_empty()",
        "noevia-model-manager",
        "actual_framework_query_redirect_method_and_auth_contract",
        false,
    )
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next();
    let metadata_only = mode.as_deref() == Some("--metadata-only");
    let repair_tail = mode.as_deref() == Some("--http-lifecycle-only");
    let auth_only = mode.as_deref() == Some("--auth-only");
    let repair_only = mode.as_deref() == Some("--repair-only") || repair_tail;
    match mode.as_deref() {
        None
        | Some("--metadata-only" | "--repair-only" | "--http-lifecycle-only" | "--auth-only") => (),
        _ => return Err("unexpected mutation group".into()),
    };
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    if auth_only {
        auth_mutation()?;
        return Ok(());
    }
    let root = Path::new("bins/noevia-model-manager/src/sections");
    if !metadata_only && !repair_only {
        for (file, before, after) in [
            (
                "ini.rs",
                "for (key, val) in &self.defaults.values",
                "for (key, val) in self.defaults.values.iter().take(0)",
            ),
            (
                "ini.rs",
                "matches!(kind(key), Some(\"bool\" | \"flag\"))",
                "matches!(kind(key), Some(\"bool\" | \"flag\")) || low == \"1\" || low == \"0\"",
            ),
            (
                "mod.rs",
                "if self.models.file(relative, false, budget)?.is_some()",
                "if false && self.models.file(relative, false, budget)?.is_some()",
            ),
            ("mod.rs", "low.contains(\"imatrix\")", "false"),
            ("mod.rs", "if defaults || current.is_none()", "if defaults"),
            (
                "defaults.rs",
                "hints.insert(0,if let Some(total)",
                "hints.push(if let Some(total)",
            ),
        ] {
            mutation(
                &root.join(file),
                before,
                after,
                "noevia-model-manager",
                "complete_actual_python_responses",
                false,
            )?;
        }
        auth_mutation()?;
        mutation(
            Path::new("crates/gguf/src/raw.rs"),
            "if n > individual\n                || reservation.is_none_or",
            "if false && n > individual\n                || reservation.is_none_or",
            "gguf",
            "tighter_components_refuse_distinctly_without_changing_default_reader",
            true,
        )?;
    }
    if !repair_only {
        for (before, after) in [
            ("*nodes > 1024", "*nodes > usize::MAX"),
            ("depth > 8", "depth > usize::MAX"),
            ("text.is_ascii()", "true"),
            ("size > limit", "size > usize::MAX"),
        ] {
            mutation(
                Path::new("crates/gguf/src/lib.rs"),
                before,
                after,
                "gguf",
                "bounded_metadata_text_preflights_dialect_depth_nodes_and_output",
                true,
            )?;
        }
    }
    if !metadata_only {
        if !repair_tail {
            mutation(
                &root.join("defaults.rs"),
                "gguf_suffix(&e.name)",
                "e.name.to_lowercase().ends_with(\".gguf\")",
                "noevia-model-manager",
                "complete_actual_python_responses",
                false,
            )?;
            mutation(
                &root.join("defaults.rs"),
                "Some(integer.clone())",
                "if matches!(integer, gguf::PyInt::Big(_)) { None } else { Some(integer.clone()) }",
                "noevia-model-manager",
                "complete_actual_python_responses",
                false,
            )?;
        }
        mutation(
            Path::new("bins/noevia-model-manager/src/lib.rs"),
            "Ok(name) if name.contains('/')",
            "Ok(name) if name.contains('/') && !query.contains(\"defaults=bad\")",
            "noevia-model-manager",
            "actual_framework_query_redirect_method_and_auth_contract",
            false,
        )?;
        mutation(
            Path::new("bins/noevia-model-manager/src/main.rs"),
            "output.write_all(b\"native-logs\\n\")?;",
            "println!(\"native-logs\");",
            "noevia-model-manager",
            "capability_closed_stdout_returns_error_without_panic_or_child",
            false,
        )?;
        mutation(Path::new("bins/noevia-model-manager/tests/lifecycle.rs"), "CHILD_PORTS.lock().unwrap()", "{ static OTHER: std::sync::Mutex<()> = std::sync::Mutex::new(()); OTHER.lock().unwrap() }", "noevia-model-manager", "child_port_guard_excludes_second_fixed_port_owner", false)?;
    }
    if let Some(services) =
        std::env::var_os("NOEVIA_SECTIONS_SERVICES").filter(|_| !metadata_only && !repair_only)
    {
        let source = Path::new(&services).join("model-manager/app/main.py");
        let original = fs::read_to_string(&source)?;
        let before = "request.method == \"GET\"";
        if original.matches(before).count() != 1 {
            return Err("nonunique ownership mutation target".into());
        }
        fs::write(
            &source,
            original.replace(before, "request.method in {\"GET\", \"PUT\"}"),
        )?;
        let result = Command::new(std::env::var_os("PYTHON").ok_or("PYTHON required")?)
            .current_dir(&services)
            .args([
                "-m",
                "pytest",
                "-q",
                "model-manager/tests/test_api_contract.py",
                "-k",
                "native_sections_guard",
            ])
            .output();
        fs::write(source, original)?;
        let result = result?;
        let stdout = String::from_utf8_lossy(&result.stdout);
        if result.status.success()
            || !stdout.contains("FAILED")
            || !stdout.contains("test_native_sections_guard")
        {
            return Err("Python ownership mutation did not reach assertion".into());
        }
        println!("detected method-aware Python ownership assertion");
    }
    Ok(())
}
