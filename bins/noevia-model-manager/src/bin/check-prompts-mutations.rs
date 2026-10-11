//! Run only in an isolated checkout; every mutant must compile and fail a named assertion.
#![forbid(unsafe_code)]
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
#[path = "support/prompts_process.rs"]
mod process;
struct Restore {
    path: PathBuf,
    bytes: Vec<u8>,
}
impl Drop for Restore {
    fn drop(&mut self) {
        let _ = fs::write(&self.path, &self.bytes);
    }
}
fn mutation(
    file: &str,
    before: &str,
    after: &str,
    test: &str,
    kind: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new("bins/noevia-model-manager/src").join(file);
    let bytes = fs::read(&path)?;
    let original = std::str::from_utf8(&bytes)?;
    if original.matches(before).count() != 1 {
        return Err(format!("nonunique target: {test}: {before}").into());
    }
    let mutant = original.replace(before, after);
    let restore = Restore {
        path: path.clone(),
        bytes,
    };
    fs::write(&path, mutant)?;
    let mut command = Command::new("cargo");
    command.args([
        "test",
        "--locked",
        "-q",
        "-p",
        "noevia-model-manager",
        "--features",
        "native-prompts",
    ]);
    match kind {
        "lib" => {
            command.arg("--lib");
        }
        "main" => {
            command.args(["--bin", "noevia-model-manager"]);
        }
        _ => return Err("unknown test target".into()),
    };
    command.arg(test);
    let outcome = process::bounded(command, Vec::new(), Duration::from_secs(60));
    fs::write(&restore.path, &restore.bytes)?;
    drop(restore);
    let outcome = outcome?;
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    if outcome.status.success() || !stdout.contains("test result: FAILED") || !stdout.contains(test)
    {
        return Err(format!(
            "mutant did not reach required assertion: {test}\n{stdout}\n{}",
            String::from_utf8_lossy(&outcome.stderr)
        )
        .into());
    }
    println!("detected {file}: {before} ({test})");
    Ok(())
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mode = args.next();
    if mode
        .as_deref()
        .is_some_and(|m| !matches!(m, "--schema-only" | "--shutdown-only" | "--raw-only"))
        || args.next().is_some()
    {
        return Err("unexpected arguments".into());
    }
    if mode.is_none() {
        for (file,before,after,test,kind) in [
        ("lib.rs","if path != \"/api/v1/health\" && !state.token.is_empty()","if path != \"/api/v1/health\" && !path.starts_with(\"/api/v1/prompts\") && !state.token.is_empty()","auth_precedes_method_path_body_and_no_fallback","lib"),
        ("prompts/store.rs","if length > 0 {","if false && length > 0 {","independent_seed_commits_restart_and_deleted_seed_retention","lib"),
        ("prompts/literals.rs","Bench: coding","Bench: codinx","literal_bytes_and_python_coercion_from_real_oracle","lib"),
        ("prompts/store.rs","ORDER BY name COLLATE NOCASE LIMIT ?","ORDER BY name LIMIT ?","actual_python_seed_flags_case_variants_and_nocase_ties","lib"),
        ("prompts/text.rs","Value::Bool(true) => out.add(\"True\")","Value::Bool(true) => out.add(\"true\")","literal_bytes_and_python_coercion_from_real_oracle","lib"),
        ("prompts/mod.rs","NAME_CAP: usize = 4096","NAME_CAP: usize = 8192","resource_caps_apply_before_native_text_and_store","lib"),
        ("prompts/mod.rs","b.eq_ignore_ascii_case(b\"%2f\")","b.eq_ignore_ascii_case(b\"%2e\")","whole_http_actual_python_corpus_and_explicit_narrowing","lib"),
        ("prompts/store.rs","self.db.progress_handler(\n            1,","self.db.progress_handler(\n            1000,","cancelled_busy_delete_cannot_commit_after_unlock","lib"),
        ("prompts/store.rs","let id = match op {","sql(self.db.execute_batch(\"BEGIN\"))?;\n        let id = match op {","failed_readback_does_not_undo_committed_post_or_delete","lib"),
        ("main.rs","key != \"DATA_DIR\" && key.eq_ignore_ascii_case(\"DATA_DIR\")","false && key != \"DATA_DIR\" && key.eq_ignore_ascii_case(\"DATA_DIR\")","canonical_root_default_and_alias_policy_is_pure","main"),
    ] {mutation(file,before,after,test,kind)?;}
    }
    if mode.is_none() || mode.as_deref() == Some("--schema-only") {
        mutation(
            "prompts/store.rs",
            "Ok(autoincrement_keyword(schema))",
            "Ok(schema.to_ascii_uppercase().contains(\"AUTOINCREMENT\"))",
            "schema_comments_strings_and_quoted_names_cannot_fake_autoincrement",
            "lib",
        )?;
    }
    if mode.is_none() || mode.as_deref() == Some("--shutdown-only") {
        for (before, after) in [
            (
                "let mut warned = false;",
                "let mut warned = false; drop(lease.take());",
            ),
            (
                "let mut failure = child.request_kill().err();",
                "child.request_kill()?; let mut failure = None;",
            ),
            (
                "Ok(Err(error)) => {",
                "Ok(Err(error)) => {return Err(error); #[allow(unreachable_code)]",
            ),
            (
                "Err(_) => {}",
                "Err(_) => {return Err(io::Error::other(\"synthetic premature timeout\"));}",
            ),
        ] {
            mutation(
                "child_lifecycle.rs",
                before,
                after,
                "kill_failure_wait_error_and_timeout_cannot_release_lease_before_reap",
                "main",
            )?;
        }
    }
    if mode.is_none() || mode.as_deref() == Some("--raw-only") {
        for (file, before, after, test) in [
            (
                "prompts/encoding.rs",
                "(Encoding::Utf8, rest)",
                "(Encoding::Utf8, bytes)",
                "malformed_json_diagnostics_match_actual_fastapi_character_offsets",
            ),
            (
                "lib.rs",
                "None => true,",
                "None => false,",
                "whole_http_actual_python_corpus_and_explicit_narrowing",
            ),
            (
                "prompts/text.rs",
                "if contains_nonfinite(value) {",
                "if false && contains_nonfinite(value) {",
                "explicit_raw_input_restrictions_have_actual_oracle_proof_and_no_writes",
            ),
        ] {
            mutation(file, before, after, test, "lib")?;
        }
    }
    Ok(())
}
