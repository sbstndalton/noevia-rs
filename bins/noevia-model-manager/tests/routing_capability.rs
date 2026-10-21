#![allow(clippy::unwrap_used, clippy::panic)]
#[path = "support/retired_selectors.rs"]
mod retired_selectors;
#[cfg(not(feature = "native-routing"))]
#[test]
fn missing_mandatory_capability_refuses_before_startup() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_noevia-model-manager"))
        .env_clear()
        .env("MODEL_MANAGER_ROUTING_IMPL", "rust")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("mandatory native-routing capability is not compiled"),
        "{error}"
    );
}

/// rs#266: the feature-gated tests in this file vanish silently when `native-routing` is removed
/// or renamed (their `cfg` is then simply false). This test carries no feature gate, so it fails
/// loudly in that case: the manifest must still declare the feature, and it must still pull in
/// the telemetry feature (which brings the sampler the restart-error test expects).
#[test]
fn native_routing_feature_is_declared() {
    let manifest = include_str!("../Cargo.toml");
    let features = manifest
        .split("\n[")
        .find(|section| section.starts_with("features]"))
        .unwrap_or_else(|| panic!("Cargo.toml has no [features] table"));
    let declaration = features
        .lines()
        .find(|line| line.starts_with("native-routing = ["))
        .unwrap_or_else(|| panic!("the native-routing feature is no longer declared"));
    assert!(
        declaration.contains("native-telemetry"),
        "native-routing no longer includes the sampler paths: {declaration}"
    );
}

/// noevia#252: `native-routing` compiles the restart paths the README describes: the restart and
/// test routes (`native-backend-control`) and the sampler that owns the restart-error store and
/// answers `GET /api/v1/backends` (`native-backends`). Both are reached through other features
/// of `native-routing`, so the shipping binary reports them without being named in the list.
#[cfg(feature = "native-routing")]
#[test]
fn native_routing_compiles_the_restart_error_paths() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_noevia-model-manager"))
        .arg("--features")
        .env_clear()
        .output()
        .unwrap();
    assert!(output.status.success());
    let compiled = String::from_utf8_lossy(&output.stdout);
    let compiled: Vec<&str> = compiled.lines().collect();
    for feature in ["native-backend-control", "native-backends"] {
        assert!(
            compiled.contains(&feature),
            "{feature} is not compiled: {compiled:?}"
        );
    }
}

#[cfg(all(feature = "native-routing", unix))]
#[test]
fn bad_prerequisites_never_reap_orphans_seed_embedding_or_initialize_a_store() {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("docker.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let trusted = dir.path().join("trusted.json");
    for (contents, timezone) in [
        (None, "UTC"),
        (Some("{bad"), "UTC"),
        (Some("{}"), "UTC"),
        (Some(r#"{"quantisers":[],"labs":[]}"#), "UTC"),
        (
            Some(r#"{"quantisers":["qa"],"labs":["qa"]}"#),
            "qa-invalid-timezone",
        ),
    ] {
        if let Some(contents) = contents {
            std::fs::write(&trusted, contents).unwrap();
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_noevia-model-manager"));
        command
            .env_clear()
            .env("MODEL_MANAGER_ROUTING_IMPL", "rust");
        for key in retired_selectors::RETIRED_SELECTORS {
            command.env(key, "rust");
        }
        command
            .env("SEARCH_TRUSTED_PATH", &trusted)
            .env("TZ", timezone)
            .env(
                "MODEL_LOADER_TOKEN",
                "qa-model-loader-token-0123456789abcdef",
            )
            .env("DOCKER_HOST", format!("unix://{}", socket.display()))
            .env("DATA_DIR", dir.path().join("data"))
            .env("MODELS_DIR", dir.path().join("models"))
            .env("MODELS_INI_PATH", dir.path().join("models/models.ini"))
            .env("EMBED_CONTAINER", "qa-embed")
            .env("EMBED_PRESET_PATH", dir.path().join("embedding.ini"))
            .env("EMBEDDING_MODEL", "qa-model")
            .env("EMBED_MODEL_FILE", "qa-file")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("prerequisite refusal reached startup work");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        if timezone == "UTC" {
            assert!(error.contains("trusted publisher"), "{error}");
        }
        assert!(listener.accept().is_err(), "Docker startup work occurred");
        assert!(!dir.path().join("data").exists(), "store initialized");
        assert!(
            !dir.path().join("embedding.ini").exists(),
            "embedding preset seeded"
        );
    }
}
