//! sbstndalton/noevia#1071: NOEVIA_OCR_IMPL and DOCX_TEXT_IMPL are retired. Whatever a leftover
//! environment says, the binary either serves natively (native-ocr builds) or refuses (featureless
//! builds); it never starts the Python HTTP server. A planted `server.py` and a `python`/`python3`
//! shim on PATH that would leave a marker prove the retired owner never runs. Synthetic only.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const OCR_WARNING: &str = "NOEVIA_OCR_IMPL is retired; the native server is always used";
const DOCX_WARNING: &str = "DOCX_TEXT_IMPL is retired; the native DOCX reader is always used";

struct Planted {
    root: tempfile::TempDir,
}

impl Planted {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let marker = root.path().join("python-ran");
        for name in ["python", "python3"] {
            let shim = bin.join(name);
            std::fs::write(
                &shim,
                format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker.display()),
            )
            .unwrap();
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            root.path().join("server.py"),
            format!("open({:?}, 'w').close()\n", marker.display().to_string()),
        )
        .unwrap();
        Self { root }
    }
    fn command(&self, port: u16, selectors: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_noevia-ocr"));
        let path = format!(
            "{}:{}",
            self.root.path().join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command
            .current_dir(self.root.path())
            .env_remove("NOEVIA_OCR_IMPL")
            .env_remove("DOCX_TEXT_IMPL")
            .env("PATH", path)
            .env("NOEVIA_OCR_LISTEN", format!("127.0.0.1:{port}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        for (key, value) in selectors {
            command.env(key, value);
        }
        command
    }
    fn python_ran(&self) -> bool {
        self.root.path().join("python-ran").exists()
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

const CASES: [&[(&str, &str)]; 7] = [
    &[],
    &[("NOEVIA_OCR_IMPL", "")],
    &[("NOEVIA_OCR_IMPL", "rust")],
    &[("NOEVIA_OCR_IMPL", "python")],
    &[("NOEVIA_OCR_IMPL", "invalid")],
    &[("DOCX_TEXT_IMPL", "python")],
    &[("NOEVIA_OCR_IMPL", "python"), ("DOCX_TEXT_IMPL", "python")],
];

fn expected_warnings(selectors: &[(&str, &str)]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for (key, value) in selectors {
        let value = value.trim();
        if value.is_empty() || value.eq_ignore_ascii_case("rust") {
            continue;
        }
        out.push(if *key == "NOEVIA_OCR_IMPL" {
            OCR_WARNING
        } else {
            DOCX_WARNING
        });
    }
    out
}

#[cfg(feature = "native-ocr")]
#[test]
fn every_leftover_selector_gets_the_native_server() {
    use std::io::Write;
    for selectors in CASES {
        let planted = Planted::new();
        let port = free_port();
        let mut child = planted.command(port, selectors).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut reply = String::new();
        loop {
            assert!(
                Instant::now() < deadline,
                "{selectors:?}: native OCR did not answer /health"
            );
            if let Ok(mut socket) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                socket
                    .write_all(
                        b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                let _ = socket.take(4096).read_to_string(&mut reply);
                break;
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "{selectors:?}: exited early"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        child.kill().unwrap();
        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            reply.starts_with("HTTP/1.1 200"),
            "{selectors:?}: native server must answer: {reply}"
        );
        assert!(
            !planted.python_ran(),
            "{selectors:?}: the retired Python owner ran"
        );
        let expected = expected_warnings(selectors);
        for warning in [OCR_WARNING, DOCX_WARNING] {
            assert_eq!(
                stderr.matches(warning).count(),
                usize::from(expected.contains(&warning)),
                "{selectors:?}: {stderr}"
            );
        }
        assert!(
            !stderr.contains("invalid"),
            "{selectors:?}: the value is never echoed"
        );
    }
}

#[cfg(not(feature = "native-ocr"))]
#[test]
fn a_featureless_build_refuses_whatever_the_selector_says() {
    for selectors in CASES {
        let planted = Planted::new();
        let mut child = planted.command(free_port(), selectors).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "{selectors:?}: did not refuse");
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(
            !status.success(),
            "{selectors:?}: featureless build started"
        );
        assert!(
            stderr.contains("native-ocr feature is not enabled"),
            "{selectors:?}: {stderr}"
        );
        assert!(
            !planted.python_ran(),
            "{selectors:?}: the retired Python owner ran"
        );
        let expected = expected_warnings(selectors);
        for warning in [OCR_WARNING, DOCX_WARNING] {
            assert_eq!(
                stderr.matches(warning).count(),
                usize::from(expected.contains(&warning)),
                "{selectors:?}: {stderr}"
            );
        }
    }
}
