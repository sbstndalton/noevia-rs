#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
#[test]
fn default_python_owner_exec_preserves_pid_and_exit_status() {
    let root = tempfile::tempdir().unwrap();
    let python = std::process::Command::new("python3")
        .args(["-c", "import sys;print(sys.executable)"])
        .output()
        .unwrap();
    assert!(python.status.success());
    std::os::unix::fs::symlink(
        String::from_utf8(python.stdout).unwrap().trim(),
        root.path().join("python"),
    )
    .unwrap();
    std::fs::write(
        root.path().join("server.py"),
        "import os,sys;print(os.getpid(),flush=True);sys.exit(7)",
    )
    .unwrap();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_noevia-docling"))
        .env_remove("NOEVIA_DOCLING_IMPL")
        .env(
            "PATH",
            format!(
                "{}:{}",
                root.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .current_dir(root.path())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap(),
        pid,
        "Python owner must replace native supervisor PID"
    );
}
#[cfg(not(feature = "native-docling"))]
#[tokio::test]
async fn native_selection_without_feature_refuses_before_child_spawn() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("isolated_worker.py"),
        "open('spawned','w').write('bad')",
    )
    .unwrap();
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_noevia-docling"))
        .env("NOEVIA_DOCLING_IMPL", "rust")
        .env("DOCLING_PORT", "0")
        .current_dir(root.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let output = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait_with_output())
        .await
        .expect("feature omission did not refuse promptly")
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("native-docling feature required"));
    assert!(!root.path().join("spawned").exists());
}

#[cfg(feature = "native-docling")]
struct Running(tokio::process::Child);
#[cfg(feature = "native-docling")]
impl Drop for Running {
    fn drop(&mut self) {
        if let Some(pid) = self.0.id().and_then(|v| i32::try_from(v).ok()) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }
}
#[cfg(feature = "native-docling")]
#[tokio::test]
async fn native_binary_real_worker_and_graceful_shutdown() {
    use std::{path::PathBuf, process::Stdio, time::Duration};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let root = tempfile::tempdir().unwrap();
    let source = PathBuf::from(
        std::env::var("NOEVIA_DOCLING_REFERENCE").expect("required pinned worker source"),
    );
    std::fs::copy(
        source.join("isolated_worker.py"),
        root.path().join("isolated_worker.py"),
    )
    .unwrap();
    std::fs::write(
        root.path().join("extract.py"),
        "import os\ndef extract(path,name):\n return {'text':'synthetic','pid':os.getpid()}\n",
    )
    .unwrap();
    let python = std::process::Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .unwrap();
    assert!(python.status.success());
    std::os::unix::fs::symlink(
        String::from_utf8(python.stdout).unwrap().trim(),
        root.path().join("python"),
    )
    .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = Running(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_noevia-docling"))
            .env("NOEVIA_DOCLING_IMPL", "rust")
            .env("DOCLING_PORT", port.to_string())
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    root.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .current_dir(root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut ready = tokio::io::BufReader::new(child.0.stderr.take().unwrap()).lines();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), ready.next_line())
            .await
            .unwrap()
            .unwrap()
            .as_deref(),
        Some("Docling Rust front ready")
    );
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    socket.write_all(b"POST /extract HTTP/1.0\r\nHost: localhost\r\nX-Document-Name: x.md\r\nContent-Length: 1\r\n\r\nx").await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(bytes.starts_with(b"HTTP/1.0 200"));
    let split = bytes.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
    let result: serde_json::Value = serde_json::from_slice(&bytes[split + 4..]).unwrap();
    assert_eq!(result["text"], "synthetic");
    let worker = i32::try_from(result["pid"].as_i64().unwrap()).unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.0.id().unwrap()).unwrap()),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(3), child.0.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    assert!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(worker), None).is_err(),
        "warm worker outlived native parent"
    );
}
