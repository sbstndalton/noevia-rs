#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
//! sbstndalton/noevia#1071: the native server is the only owner. NOEVIA_DOCLING_IMPL is retired:
//! an older environment file's `python` (or any other leftover) earns one warning and still gets
//! the native server, never the Python HTTP server, and a build without the capability refuses
//! before it listens or starts a worker, whatever the selector says.

const WARNING: &str = "NOEVIA_DOCLING_IMPL is retired; the native server is always used";

/// A directory where every way back to the retired Python owner leaves a marker: a `server.py`,
/// and a `python` on PATH that records any start before it runs the real interpreter.
fn planted(root: &std::path::Path) -> String {
    std::fs::write(
        root.join("server.py"),
        "open('python-owner-started','w').write('bad')",
    )
    .unwrap();
    let python = std::process::Command::new("python3")
        .args(["-c", "import sys;print(sys.executable)"])
        .output()
        .unwrap();
    assert!(python.status.success());
    let real = String::from_utf8(python.stdout).unwrap().trim().to_owned();
    let shim = root.join("python");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in *server.py) : > '{}/python-owner-started';; esac; done\nexec '{real}' \"$@\"\n",
            root.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:{}", root.display(), std::env::var("PATH").unwrap())
}

#[cfg(not(feature = "native-docling"))]
#[tokio::test]
async fn featureless_build_refuses_before_any_child_for_every_selector_value() {
    for selector in [
        None,
        Some("rust"),
        Some(""),
        Some("python"),
        Some("PYTHON"),
        Some("junk"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = planted(root.path());
        std::fs::write(
            root.path().join("isolated_worker.py"),
            "open('spawned','w').write('bad')",
        )
        .unwrap();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_noevia-docling"));
        match selector {
            Some(value) => command.env("NOEVIA_DOCLING_IMPL", value),
            None => command.env_remove("NOEVIA_DOCLING_IMPL"),
        };
        let child = command
            .env("DOCLING_PORT", "0")
            .env("PATH", path)
            .current_dir(root.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(2), child.wait_with_output())
                .await
                .expect("feature omission did not refuse promptly")
                .unwrap();
        assert!(!output.status.success(), "{selector:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("native-docling feature required"),
            "{selector:?}: {stderr}"
        );
        let leftover = selector.is_some_and(|v| !v.is_empty() && v != "rust");
        assert_eq!(stderr.contains(WARNING), leftover, "{selector:?}: {stderr}");
        assert!(!stderr.contains("must be python or rust"), "{selector:?}");
        assert!(!root.path().join("spawned").exists(), "{selector:?}");
        assert!(
            !root.path().join("python-owner-started").exists(),
            "{selector:?}"
        );
    }
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
    let source = PathBuf::from(
        std::env::var("NOEVIA_DOCLING_REFERENCE").expect("required pinned worker source"),
    );
    // No selector (the default now), the old native value, and an older .env's `python`: all
    // three get the native server; only the leftover `python` earns the warning first.
    for selector in [None, Some("rust"), Some("python")] {
        let root = tempfile::tempdir().unwrap();
        let path = planted(root.path());
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
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_noevia-docling"));
        match selector {
            Some(value) => command.env("NOEVIA_DOCLING_IMPL", value),
            None => command.env_remove("NOEVIA_DOCLING_IMPL"),
        };
        let mut child = Running(
            command
                .env("DOCLING_PORT", port.to_string())
                .env("PATH", path)
                .current_dir(root.path())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut lines = tokio::io::BufReader::new(child.0.stderr.take().unwrap()).lines();
        let mut first = vec![];
        for _ in 0..if selector == Some("python") { 2 } else { 1 } {
            first.push(
                tokio::time::timeout(Duration::from_secs(3), lines.next_line())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_or_default(),
            );
        }
        let mut expected = vec![];
        if selector == Some("python") {
            expected.push(WARNING);
        }
        expected.push("Docling Rust front ready");
        assert_eq!(first, expected, "{selector:?}");
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        socket.write_all(b"POST /extract HTTP/1.0\r\nHost: localhost\r\nX-Document-Name: x.docx\r\nContent-Length: 1\r\n\r\nx").await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert!(bytes.starts_with(b"HTTP/1.0 200"), "{selector:?}");
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
        assert!(
            !root.path().join("python-owner-started").exists(),
            "{selector:?}: the retired Python owner started"
        );
    }
}
