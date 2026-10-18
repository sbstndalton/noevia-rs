#![allow(clippy::unwrap_used)]
#[test]
fn explicit_routing_refuses_before_child_when_compilation_or_helpers_are_missing() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_noevia-model-manager"))
        .env_clear()
        .env("MODEL_MANAGER_ROUTING_IMPL", "rust")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("native routing requires compiled native-routing")
            || error.contains(
                "native routing requires every public owner and private helper selected natively"
            ),
        "{error}"
    );
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
        for key in noevia_model_manager::routing_cmd::REQUIRED {
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
