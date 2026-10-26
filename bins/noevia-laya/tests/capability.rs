#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use std::process::Command;
#[cfg(feature = "native-laya")]
#[path = "../../../crates/sensitivity-service/tests/support/oracle.rs"]
mod oracle;
#[cfg(feature = "native-laya")]
#[test]
fn actual_compiled_front_serves_with_merged_synthetic_engine_and_stops() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    oracle::prepare(&oracle::reference(), directory.path()).unwrap();
    let interpreter = Command::new("python3")
        .args(["-c", "import sys;print(sys.executable)"])
        .output()
        .unwrap();
    assert!(interpreter.status.success());
    let interpreter = String::from_utf8(interpreter.stdout).unwrap();
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let python = directory.path().join("python");
    std::fs::write(
        &python,
        format!(
            "#!/bin/sh\nexec {} {} --engine-worker\n",
            quote(interpreter.trim()),
            quote(&directory.path().join("server.py").to_string_lossy())
        ),
    )
    .unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700)).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_noevia-laya"))
        .env_remove("NOEVIA_LAYA_IMPL")
        .env("PATH", directory.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    struct Stop(std::process::Child);
    impl Drop for Stop {
        fn drop(&mut self) {
            if let Ok(pid) = i32::try_from(self.0.id()) {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGTERM,
                );
            }
            for _ in 0..100 {
                if matches!(self.0.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Stop(child);
    let startup = std::time::Instant::now();
    let ready = loop {
        if let Ok(value) = oracle::exchange(8040, "GET", "/health", None, &[]) {
            break value;
        }
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "native front exited during startup"
        );
        assert!(startup.elapsed() < std::time::Duration::from_secs(3));
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(ready["body"], serde_json::json!({"ready":true}));
    let body = oracle::body("ok");
    let response = oracle::exchange(
        8040,
        "POST",
        "/v1/decisions",
        Some(&body.len().to_string()),
        &body,
    )
    .unwrap();
    assert_eq!(response["status"], 200);
    assert_eq!(response["body"]["selected"], "continue");
    let response = oracle::exchange(8040, "POST", "/v1/decisions", Some("2"), b"{}").unwrap();
    assert_eq!(response["status"], 422);
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.0.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    assert!(child.0.wait().unwrap().success());
    for pid in std::fs::read_to_string(directory.path().join("starts"))
        .unwrap()
        .lines()
    {
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid.parse().unwrap()), None).is_err(),
            "native binary orphaned retained worker"
        );
    }
    // A termination during model startup must also close owned worker groups.
    std::fs::write(directory.path().join("delay-startup"), b"2").unwrap();
    let mut startup_child = Stop(
        Command::new(env!("CARGO_BIN_EXE_noevia-laya"))
            .env("NOEVIA_LAYA_IMPL", "rust")
            .env("PATH", directory.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = std::time::Instant::now();
    loop {
        if std::fs::read_to_string(directory.path().join("starts"))
            .unwrap()
            .lines()
            .count()
            == 2
        {
            break;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(startup_child.0.id()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    assert!(startup_child.0.wait().unwrap().success());
    std::thread::sleep(std::time::Duration::from_millis(100));
    for pid in std::fs::read_to_string(directory.path().join("starts"))
        .unwrap()
        .lines()
    {
        let pid: i32 = pid.parse().unwrap();
        #[cfg(target_os = "linux")]
        {
            let status = std::fs::read_to_string(format!("/proc/{pid}/stat"));
            assert!(
                status.is_err() || status.unwrap().split_whitespace().nth(2) == Some("Z"),
                "startup orphan survived"
            );
        }
        #[cfg(not(target_os = "linux"))]
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err(),
            "startup orphan survived"
        );
    }
}
#[test]
fn actual_binary_refuses_legacy_http_and_missing_native_capability() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("spawned");
    // This synthetic interpreter records whether the retained engine worker
    // was requested; invoking the old Python HTTP server is never allowed.
    let python = directory.path().join("python");
    std::fs::write(
        &python,
        format!(
            "#!/bin/sh\nprintf '%s' \"$*\" > '{}'\nexit 17\n",
            marker.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700)).unwrap();
    let binary = env!("CARGO_BIN_EXE_noevia-laya");
    use std::os::unix::ffi::OsStringExt;
    let result = Command::new(binary)
        .env("NOEVIA_LAYA_IMPL", std::ffi::OsString::from_vec(vec![0xff]))
        .env("PATH", directory.path())
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("must be UTF-8 rust"));
    assert!(!marker.exists(), "invalid selector executed interpreter");

    for selector in ["python", "unknown", "", "Rust"] {
        let result = Command::new(binary)
            .env("NOEVIA_LAYA_IMPL", selector)
            .env("PATH", directory.path())
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("must be rust when set"));
        assert!(!marker.exists(), "invalid selector executed interpreter");
    }

    for selector in [None, Some("rust")] {
        let mut command = Command::new(binary);
        command
            .env_remove("NOEVIA_LAYA_IMPL")
            .env("PATH", directory.path());
        if let Some(selector) = selector {
            command.env("NOEVIA_LAYA_IMPL", selector);
        }
        let result = command.output().unwrap();
        assert!(!result.status.success());
        if cfg!(feature = "native-laya") {
            assert!(!String::from_utf8_lossy(&result.stderr).contains("feature required"));
            assert_eq!(
                std::fs::read_to_string(&marker).unwrap(),
                "/service/server.py --engine-worker",
                "native failure fell back to the legacy HTTP owner"
            );
            std::fs::remove_file(&marker).unwrap();
        } else {
            assert!(
                String::from_utf8_lossy(&result.stderr).contains("native-laya feature required")
            );
            assert!(!marker.exists(), "featureless binary spawned interpreter");
        }
    }
}
