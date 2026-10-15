//! Run only in an isolated checkout; every mutant must compile and fail a named assertion.
#![forbid(unsafe_code)]
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
#[path = "support/prompts_process.rs"]
mod process;
fn source(path: &Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(1_048_577)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 1_048_576 {
        return Err("mutation source cap".into());
    }
    Ok(bytes)
}
struct Assertion {
    path: String,
    line: usize,
}
fn assertions(
    file: &str,
    before: &str,
    test: &str,
) -> Result<Vec<Assertion>, Box<dyn std::error::Error>> {
    let (path, anchors): (&str, &[&str]) = match test {
        "auth_precedes_method_path_body_and_no_fallback" => ("prompts/tests.rs", &["assert_eq!(\n            request(state.clone(), method, path, body, false).await,"]),
        "independent_seed_commits_restart_and_deleted_seed_retention" => ("prompts/store_tests.rs", &["assert_eq!(s.list().unwrap().len(), 3);"]),
        "literal_bytes_and_python_coercion_from_real_oracle" if file == "prompts/literals.rs" => ("prompts/tests.rs", &["assert_eq!(*name, literal[0].as_str().unwrap());"]),
        "literal_bytes_and_python_coercion_from_real_oracle" if file == "prompts/text.rs" && before == "Value::Bool(true) => out.add(\"True\")" => ("prompts/tests.rs", &["assert_eq!(\n                serde_json::json!([fields.0, fields.1]),", "assert_eq!(\n                result.unwrap(),\n                (\n                    prompt[\"name\"].as_str().unwrap().to_owned(),"]),
        "literal_bytes_and_python_coercion_from_real_oracle" => ("prompts/tests.rs", &["assert_eq!(\n                serde_json::json!([fields.0, fields.1]),"]),
        "actual_python_seed_flags_case_variants_and_nocase_ties" => ("prompts/store_tests.rs", &["assert_eq!(run(&mut s, Operation::List).unwrap(), case[\"expected\"]);"]),
        "resource_caps_apply_before_native_text_and_store" => ("prompts/tests.rs", &["assert_eq!(\n        (NAME_CAP, BODY_CAP, ROW_CAP, RESPONSE_CAP),"]),
        "whole_http_actual_python_corpus_and_explicit_narrowing" if file == "prompts/mod.rs" => ("prompts/tests.rs", &["assert_eq!(response.0, 400, \"{q}\");"]),
        "whole_http_actual_python_corpus_and_explicit_narrowing" => ("prompts/tests.rs", &["assert_eq!(\n            response.0,\n            case[\"expected\"][\"status\"].as_u64().unwrap() as u16,"]),
        "cancelled_busy_delete_cannot_commit_after_unlock" => ("prompts/store_tests.rs", &["assert_eq!(\n        listed[\"prompts\"].as_array().unwrap().len(),"]),
        "failed_readback_does_not_undo_committed_post_or_delete" => ("prompts/store_tests.rs", &["assert_eq!(\n        observer\n            .query_row(\"SELECT name FROM prompts WHERE id=1\""]),
        "canonical_root_default_and_alias_policy_is_pure" => ("main.rs", &["assert!(\n                super::prompt_data_dir_from([(alias.into(),"]),
        "schema_comments_strings_and_quoted_names_cannot_fake_autoincrement" => ("prompts/store_tests.rs", &["assert!(\n            matches!(Store::open(root.path()), Err(Error::Store)),"]),
        "prompt_selector_preserves_unrelated_dispatch_and_auth_first" => ("prompts/tests.rs", &["assert_eq!(request(on.clone(), \"GET\", path, \"\", true).await.0, 404);"]),
        "kill_failure_wait_error_and_timeout_cannot_release_lease_before_reap" if before == "let mut warned = false;" => ("child_lifecycle.rs", &["assert!(self.lease.upgrade().is_some());\n            self.kills", "assert!(self.lease.upgrade().is_some());\n            match self.waits"]),
        "kill_failure_wait_error_and_timeout_cannot_release_lease_before_reap" => ("child_lifecycle.rs", &["assert!(child.waits.is_empty());"]),
        "malformed_json_diagnostics_match_actual_fastapi_character_offsets" => ("prompts/tests.rs", &["assert_eq!(\n            response.0,\n            case[\"expected\"][\"status\"].as_u64().unwrap() as u16,"]),
        "explicit_raw_input_restrictions_have_actual_oracle_proof_and_no_writes" => ("prompts/tests.rs", &["assert_eq!(\n            actual.0,", "assert_eq!(actual.1, case[\"request\"][\"native_refusal\"][\"body\"]);"]),
        _ => return Err("mutation has no concrete assertion binding".into()),
    };
    let bytes = source(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(path))?;
    let path = format!("bins/noevia-model-manager/src/{path}");
    let text = std::str::from_utf8(&bytes)?;
    let start = if path.ends_with("child_lifecycle.rs") && before == "let mut warned = false;" {
        0
    } else {
        text.find(&format!("fn {test}("))
            .ok_or("assertion test missing")?
    };
    let rest = text.get(start..).ok_or("assertion region")?;
    let region = if start == 0 {
        rest
    } else {
        rest.get(..rest.find("\n}\n").ok_or("assertion function end")?)
            .ok_or("assertion function")?
    };
    anchors
        .iter()
        .map(|anchor| {
            if region.matches(anchor).count() != 1 {
                return Err("nonunique concrete assertion".into());
            }
            let offset = region.find(anchor).ok_or("assertion anchor missing")?;
            let position = start + offset;
            Ok(Assertion {
                path: path.clone(),
                line: text
                    .get(..position)
                    .ok_or("assertion position")?
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count()
                    + 1,
            })
        })
        .collect()
}
fn semantic_failure(
    code: Option<i32>,
    stdout: &str,
    stderr: &str,
    test: &str,
    assertions: &[Assertion],
) -> bool {
    if code != Some(101)
        || !stdout.lines().any(|line| line == "running 1 test")
        || !stdout.contains("test result: FAILED. 0 passed; 1 failed;")
        || stdout.matches(" panicked at ").count() != 1
        || stdout.matches("test result:").count() != 1
        || stdout
            .lines()
            .rev()
            .find(|line| !line.is_empty())
            .is_none_or(|line| !line.starts_with("test result: FAILED. 0 passed; 1 failed;"))
        || stderr.lines().any(|line| {
            line.starts_with("error[")
                || (line.starts_with("error:")
                    && !line.starts_with("error: test failed, to rerun pass"))
        })
    {
        return false;
    }
    let mut failed = stdout.lines().filter_map(|line| {
        line.strip_prefix("test ")
            .and_then(|line| line.strip_suffix(" ... FAILED"))
            .or_else(|| line.strip_suffix(" --- FAILED"))
    });
    let Some(name) = failed.next() else {
        return false;
    };
    if failed.next().is_some()
        || !(name == test || name.ends_with(&format!("::{test}")))
        || !stdout.contains(&format!("---- {name} stdout ----"))
    {
        return false;
    }
    let mut lines = stdout.lines();
    while let Some(line) = lines.next() {
        if let Some(tail) = line.strip_prefix(&format!("thread '{name}'")) {
            let location = tail
                .strip_prefix(" panicked at ")
                .or_else(|| {
                    let (id, location) = tail.strip_prefix(" (")?.split_once(") panicked at ")?;
                    (!id.is_empty()
                        && id.len() <= 20
                        && id.bytes().all(|byte| byte.is_ascii_digit()))
                    .then_some(location)
                })
                .is_some_and(|location| {
                    assertions.iter().any(|assertion| {
                        location
                            .strip_prefix(&format!("{}:{}:", assertion.path, assertion.line))
                            .and_then(|column| column.strip_suffix(':'))
                            .is_some_and(|column| {
                                !column.is_empty()
                                    && column.len() <= 6
                                    && column.bytes().all(|byte| byte.is_ascii_digit())
                            })
                    })
                });
            let Some(message) = lines.next() else {
                return false;
            };
            return location
                && (message.starts_with("assertion `left == right` failed")
                    || message.starts_with("assertion failed:")
                    || message.starts_with("PROMPTS_ASSERT:schema-autoincrement "));
        }
    }
    false
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
fn mutation(
    file: &str,
    before: &str,
    after: &str,
    test: &str,
    kind: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new("bins/noevia-model-manager/src").join(file);
    let bytes = source(&path)?;
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
    let expected = assertions(file, before, test)?;
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
    if !semantic_failure(
        outcome.status.code(),
        &stdout,
        &String::from_utf8_lossy(&outcome.stderr),
        test,
        &expected,
    ) {
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
    if mode.as_deref().is_some_and(|m| {
        !matches!(
            m,
            "--schema-only"
                | "--shutdown-only"
                | "--raw-only"
                | "--ownership-only"
                | "--coercion-only"
        )
    }) || args.next().is_some()
    {
        return Err("unexpected arguments".into());
    }
    if mode.is_none() || mode.as_deref() == Some("--coercion-only") {
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
    ] {
        if mode.as_deref() == Some("--coercion-only") && before != "Value::Bool(true) => out.add(\"True\")" { continue; }
        mutation(file,before,after,test,kind)?;
    }
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
    if mode.is_none() || mode.as_deref() == Some("--ownership-only") {
        mutation(
            "prompts/mod.rs",
            "if !prompt_namespace(raw) {",
            "if false && !prompt_namespace(raw) {",
            "prompt_selector_preserves_unrelated_dispatch_and_auth_first",
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
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    #[test]
    fn premature_lease_drop_binds_kill_and_wait_assertions_only() {
        let test = "kill_failure_wait_error_and_timeout_cannot_release_lease_before_reap";
        let expected = assertions("child_lifecycle.rs", "let mut warned = false;", test).unwrap();
        assert_eq!(expected.len(), 2);
        let other = assertions(
            "child_lifecycle.rs",
            "let mut failure = child.request_kill().err();",
            test,
        )
        .unwrap();
        assert_eq!(other.len(), 1);
        let literal = assertions(
            "prompts/literals.rs",
            "Bench: coding",
            "literal_bytes_and_python_coercion_from_real_oracle",
        )
        .unwrap();
        for assertion in &expected {
            let out=format!("\nrunning 1 test\nchild_lifecycle::tests::{test} --- FAILED\n\n---- child_lifecycle::tests::{test} stdout ----\nthread 'child_lifecycle::tests::{test}' (55) panicked at {}:{}:13:\nassertion failed: self.lease.upgrade().is_some()\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured;\n",assertion.path,assertion.line);
            assert!(semantic_failure(Some(101), &out, "", test, &expected));
            assert!(!semantic_failure(Some(101), &out, "", test, &other));
            assert!(!semantic_failure(Some(101), &out, "", test, &literal));
            for (code, invalid) in [
                (Some(0), out.clone()),
                (
                    Some(101),
                    out.replace(
                        &format!("{}:{}:", assertion.path, assertion.line),
                        &format!("{}:1:", assertion.path),
                    ),
                ),
                (Some(101), out.replace(test, "other_test")),
                (
                    Some(101),
                    out.replace(
                        "assertion failed: self.lease.upgrade().is_some()",
                        "called `Result::unwrap()` on an `Err` value: setup",
                    ),
                ),
                (
                    Some(101),
                    out.replace(
                        "assertion failed: self.lease.upgrade().is_some()",
                        "fixture setup failed",
                    ),
                ),
            ] {
                assert!(!semantic_failure(code, &invalid, "", test, &expected));
            }
        }
    }
    #[test]
    fn boolean_coercion_binds_post_tuple_and_explicit_coercion_only() {
        let test = "literal_bytes_and_python_coercion_from_real_oracle";
        let expected = assertions(
            "prompts/text.rs",
            "Value::Bool(true) => out.add(\"True\")",
            test,
        )
        .unwrap();
        assert_eq!(expected.len(), 2);
        let literal = assertions("prompts/literals.rs", "Bench: coding", test).unwrap();
        assert_eq!(literal.len(), 1);
        let stderr = "error: test failed, to rerun pass -p noevia-model-manager --lib\n";
        for assertion in &expected {
            let out = format!("\nrunning 1 test\nprompts::tests::{test} --- FAILED\n\n---- prompts::tests::{test} stdout ----\nthread 'prompts::tests::{test}' (9012) panicked at {}:{}:13:\nassertion `left == right` failed\n left: true\n right: True\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 44 filtered out;\n", assertion.path, assertion.line);
            assert!(semantic_failure(Some(101), &out, stderr, test, &expected));
            assert!(!semantic_failure(Some(101), &out, stderr, test, &literal));
            for (code, invalid, error) in [
                (Some(0), out.clone(), stderr),
                (
                    Some(101),
                    out.replace(
                        &format!("{}:{}:", assertion.path, assertion.line),
                        &format!("{}:1:", assertion.path),
                    ),
                    stderr,
                ),
                (
                    Some(101),
                    out.replace(
                        "assertion `left == right` failed",
                        "called `Result::unwrap()` on an `Err` value: setup",
                    ),
                    stderr,
                ),
                (
                    Some(101),
                    out.replace("assertion `left == right` failed", "fixture setup failed"),
                    stderr,
                ),
                (Some(101), out.replace(test, "other_test"), stderr),
                (Some(101), out.clone(), "error: could not compile fixture\n"),
            ] {
                assert!(!semantic_failure(code, &invalid, error, test, &expected));
            }
        }
    }
    #[test]
    fn semantic_classifier_rejects_infrastructure_and_wrong_assertion() {
        let expected = [Assertion {
            path: "synthetic.rs".into(),
            line: 12,
        }];
        let valid = "\nrunning 1 test\ntest synthetic::target ... FAILED\n\n---- synthetic::target stdout ----\nthread 'synthetic::target' panicked at synthetic.rs:12:4:\nassertion `left == right` failed\n left: 400\n right: 401\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out;\n";
        let stderr = "error: test failed, to rerun pass -p synthetic --lib\n";
        assert!(semantic_failure(
            Some(101),
            valid,
            stderr,
            "target",
            &expected
        ));
        // Blank lines after the terminal summary are not records.
        assert!(semantic_failure(
            Some(101),
            &format!("{valid}\n\n"),
            stderr,
            "target",
            &expected
        ));
        for (code, out, err) in [
            (Some(0), valid.to_owned(), stderr.to_owned()),
            (Some(1), valid.to_owned(), stderr.to_owned()),
            (
                Some(101),
                valid.replace("synthetic.rs:12:", "synthetic.rs:13:"),
                stderr.to_owned(),
            ),
            (
                Some(101),
                valid.replace(
                    "assertion `left == right` failed",
                    "called `Result::unwrap()` on an `Err` value: dependency unavailable",
                ),
                stderr.to_owned(),
            ),
            (
                Some(101),
                valid.replace("assertion `left == right` failed", "fixture setup failed"),
                stderr.to_owned(),
            ),
            (
                Some(101),
                valid.replace(
                    "assertion `left == right` failed",
                    "child deadline exceeded",
                ),
                stderr.to_owned(),
            ),
            (
                Some(101),
                valid.replace(
                    "assertion `left == right` failed",
                    "child output limit exceeded",
                ),
                stderr.to_owned(),
            ),
            (
                Some(101),
                valid.replace("synthetic::target", "synthetic::other"),
                stderr.to_owned(),
            ),
            (
                Some(101),
                valid.to_owned(),
                "error: could not compile synthetic\n".to_owned(),
            ),
            (
                Some(101),
                format!("{valid}setup failure\n"),
                stderr.to_owned(),
            ),
            (
                Some(101),
                format!("{valid}test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out;\n"),
                stderr.to_owned(),
            ),
            (
                Some(101),
                format!("{valid}thread 'worker' panicked at setup.rs:1:1:\nunrelated panic"),
                stderr.to_owned(),
            ),
            (None, valid.to_owned(), stderr.to_owned()),
        ] {
            assert!(!semantic_failure(code, &out, &err, "target", &expected));
        }
    }
    #[test]
    fn actual_rust_assertion_output_has_exact_identity() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("synthetic.rs");
        let binary = root.path().join("synthetic-test");
        fs::write(
            &source,
            "#[test]\nfn synthetic_target() {\n    assert_eq!(400, 401);\n}\n",
        )
        .unwrap();
        let mut compile = Command::new("rustc");
        compile
            .args(["--edition=2021", "--test"])
            .arg(&source)
            .arg("-o")
            .arg(&binary);
        let compiled = process::bounded(compile, Vec::new(), Duration::from_secs(10)).unwrap();
        assert!(compiled.status.success());
        let mut run = Command::new(binary);
        run.arg("--quiet");
        let output = process::bounded(run, Vec::new(), Duration::from_secs(5)).unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        let expected = [Assertion {
            path: source.to_string_lossy().into_owned(),
            line: 3,
        }];
        assert!(
            semantic_failure(
                output.status.code(),
                &stdout,
                &stderr,
                "synthetic_target",
                &expected
            ),
            "{stdout}\n{stderr}"
        );
        let wrong = [Assertion {
            path: source.to_string_lossy().into_owned(),
            line: 2,
        }];
        assert!(!semantic_failure(
            output.status.code(),
            &stdout,
            &stderr,
            "synthetic_target",
            &wrong
        ));
    }
    #[test]
    fn concrete_mutation_assertion_sources_are_present() {
        for (file, before, test) in [
            (
                "lib.rs",
                "",
                "auth_precedes_method_path_body_and_no_fallback",
            ),
            (
                "prompts/store.rs",
                "",
                "independent_seed_commits_restart_and_deleted_seed_retention",
            ),
            (
                "prompts/literals.rs",
                "",
                "literal_bytes_and_python_coercion_from_real_oracle",
            ),
            (
                "prompts/text.rs",
                "",
                "literal_bytes_and_python_coercion_from_real_oracle",
            ),
            (
                "prompts/store.rs",
                "",
                "actual_python_seed_flags_case_variants_and_nocase_ties",
            ),
            (
                "prompts/mod.rs",
                "",
                "resource_caps_apply_before_native_text_and_store",
            ),
            (
                "prompts/mod.rs",
                "",
                "whole_http_actual_python_corpus_and_explicit_narrowing",
            ),
            (
                "lib.rs",
                "",
                "whole_http_actual_python_corpus_and_explicit_narrowing",
            ),
            (
                "prompts/store.rs",
                "",
                "cancelled_busy_delete_cannot_commit_after_unlock",
            ),
            (
                "prompts/store.rs",
                "",
                "failed_readback_does_not_undo_committed_post_or_delete",
            ),
            (
                "main.rs",
                "",
                "canonical_root_default_and_alias_policy_is_pure",
            ),
            (
                "prompts/store.rs",
                "",
                "schema_comments_strings_and_quoted_names_cannot_fake_autoincrement",
            ),
            (
                "prompts/mod.rs",
                "",
                "prompt_selector_preserves_unrelated_dispatch_and_auth_first",
            ),
            (
                "child_lifecycle.rs",
                "let mut warned = false;",
                "kill_failure_wait_error_and_timeout_cannot_release_lease_before_reap",
            ),
            (
                "child_lifecycle.rs",
                "",
                "kill_failure_wait_error_and_timeout_cannot_release_lease_before_reap",
            ),
            (
                "prompts/encoding.rs",
                "",
                "malformed_json_diagnostics_match_actual_fastapi_character_offsets",
            ),
            (
                "prompts/text.rs",
                "",
                "explicit_raw_input_restrictions_have_actual_oracle_proof_and_no_writes",
            ),
        ] {
            assert!(assertions(file, before, test).is_ok(), "{test}");
        }
    }
}
