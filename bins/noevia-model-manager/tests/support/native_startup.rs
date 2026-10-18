// Native startup ownership contract reused by the retired component selector targets.
#![allow(clippy::unwrap_used)]
pub fn refuse(keys: &[&str], zone: Option<&str>, trust: Option<&str>) {
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let data = root.path().join("unopened-data");
    let marker = root.path().join("spawned");
    let script = root.path().join("uvicorn");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    for value in ["python", "rust", "invalid", ""] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_noevia-model-manager"));
        command
            .env_clear()
            .env("PATH", root.path())
            .env("DATA_DIR", &data)
            .env("MODELS_DIR", root.path().join("models"))
            .env("MODELS_INI_PATH", root.path().join("models.ini"))
            .env(
                "DOCKER_HOST",
                format!("unix://{}", root.path().join("absent.sock").display()),
            );
        for key in keys {
            command.env(key, value);
        }
        if let Some(zone) = zone {
            command.env("TZ", zone);
        }
        if let Some(trust) = trust {
            let path = root.path().join("trusted.json");
            std::fs::write(&path, trust).unwrap();
            command.env("SEARCH_TRUSTED_PATH", &path);
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        let error = String::from_utf8_lossy(&output.stderr);
        if !cfg!(feature = "native-routing") {
            assert!(
                error.contains("mandatory native-routing capability"),
                "{error}"
            );
        } else if zone != Some("UTC") {
            assert!(error.contains("requires exact TZ=UTC"), "{error}");
        } else {
            assert!(
                error.contains("trusted") || error.contains("TRUSTED"),
                "{error}"
            );
        }
        assert!(
            !marker.exists(),
            "retired component selector spawned Python"
        );
        assert!(!data.exists(), "invalid startup reached schema/seed writes");
        assert!(
            !root.path().join("models.ini").exists(),
            "invalid startup fabricated INI"
        );
    }
}
