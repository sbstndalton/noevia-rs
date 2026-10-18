#![allow(clippy::unwrap_used, clippy::panic)]
use std::process::Command;
const KEY: &str = "MODEL_MANAGER_SAFE_DEFAULTS_IMPL";
fn run(value: Option<&str>, writer: bool, models: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_noevia-model-manager"));
    cmd.env_clear()
        .env("PATH", dir.path())
        .env("DATA_DIR", dir.path().join("data"))
        .env("MODELS_INI_PATH", dir.path().join("models.ini"))
        .env("MODELS_DIR", models)
        .env("MODEL_LOADER_TOKEN", " ");
    if let Some(value) = value {
        cmd.env(KEY, value);
    }
    if writer {
        cmd.env("MODEL_MANAGER_SECTION_WRITES_IMPL", "rust");
    }
    let output = cmd.output().unwrap();
    assert!(!output.status.success());
    assert!(!dir.path().join("models.ini").exists());
    assert!(!dir.path().join("data").exists());
    String::from_utf8_lossy(&output.stderr).into_owned()
}
#[test]
fn invalid_selector_and_missing_capabilities_refuse_before_startup() {
    for value in ["", "Rust", " rust", "invalid"] {
        let error = run(Some(value), false, "/synthetic/models");
        assert!(error.contains("must be python or rust"), "{error}");
    }
    let error = run(Some("rust"), false, "/synthetic/models");
    assert!(
        error.contains("compiled native-safe-defaults feature and native section writes"),
        "{error}"
    );
}
#[test]
fn python_default_leaves_planning_roots_to_child() {
    let error = run(None, false, "relative/models");
    assert!(
        error.contains(if cfg!(feature = "native-logs") {
            "MODEL_LOADER_TOKEN"
        } else {
            "native-logs feature is not enabled"
        }),
        "{error}"
    );
}
#[cfg(feature = "native-safe-defaults")]
#[test]
fn native_planning_requires_exact_roots_before_startup() {
    let error = run(Some("rust"), true, "relative/models");
    assert!(!error.contains("MODEL_LOADER_TOKEN"), "{error}");
    let error = run(Some("rust"), true, "/synthetic/models");
    assert!(error.contains("MODEL_LOADER_TOKEN"), "{error}");
}
