#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
//! The health probe against scripted loopback servers, and the same scenarios against the
//! reference itself: `python -c "import urllib.request; urllib.request.urlopen(URL, timeout=T)"`
//! must succeed exactly when `health::probe` does. The reference runs when
//! `DOCLING_HEALTH_ORACLE_PYTHON` names an interpreter (python3 3.12); `..._REQUIRED=1` makes a
//! missing one a failure:
//!
//!     n5run -- 'DOCLING_HEALTH_ORACLE_PYTHON=python3 DOCLING_HEALTH_ORACLE_REQUIRED=1 \
//!       cargo test --locked -p docling-service --test health'
use docling_service::health::{probe, Unhealthy};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone)]
enum Reply {
    /// Raw bytes after reading the request head; `{port}` is replaced by the server's port.
    Bytes(String),
    /// Read the request, answer nothing, hold the connection.
    Silent(Duration),
    /// Close without reading or answering.
    Close,
}

struct Server {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// Serves `script` one connection per entry, in order; the last entry repeats.
fn serve(script: Vec<Reply>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    thread::spawn(move || {
        for (index, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { return };
            let reply = script[index.min(script.len() - 1)].clone();
            let seen = seen.clone();
            thread::spawn(move || match reply {
                Reply::Close => drop(stream),
                Reply::Silent(hold) => {
                    seen.lock().unwrap().push(read_head(&mut stream));
                    thread::sleep(hold);
                }
                Reply::Bytes(bytes) => {
                    seen.lock().unwrap().push(read_head(&mut stream));
                    let bytes = bytes.replace("{port}", &port.to_string());
                    let _ = stream.write_all(bytes.as_bytes());
                    let _ = stream.flush();
                    thread::sleep(Duration::from_millis(50));
                }
            });
        }
    });
    Server { port, requests }
}

fn ok(status: &str) -> Reply {
    Reply::Bytes(format!("HTTP/1.1 {status}\r\nContent-Length: 2\r\n\r\nok"))
}

fn redirect(status: u16, location: &str) -> Reply {
    Reply::Bytes(format!(
        "HTTP/1.1 {status} Moved\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n"
    ))
}

fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

const SHORT: Duration = Duration::from_millis(400);

/// (name, script, path, healthy)
fn scenarios() -> Vec<(&'static str, Vec<Reply>, &'static str, bool)> {
    let many_headers: String = (0..101).map(|i| format!("X-{i}: v\r\n")).collect();
    let hundred_headers: String = (0..99).map(|i| format!("X-{i}: v\r\n")).collect();
    let long_header = format!("X-Long: {}\r\n", "a".repeat(70_000));
    let chain = |n: usize| -> Vec<Reply> {
        let mut script: Vec<Reply> = (1..=n)
            .map(|i| redirect(302, &format!("/hop{i}")))
            .collect();
        script.push(ok("200 OK"));
        script
    };
    vec![
        ("200", vec![ok("200 OK")], "/health", true),
        (
            "204 no content",
            vec![Reply::Bytes("HTTP/1.1 204 No Content\r\n\r\n".into())],
            "/health",
            true,
        ),
        ("299", vec![ok("299 Odd")], "/health", true),
        (
            "HTTP/1.0 200",
            vec![Reply::Bytes("HTTP/1.0 200 OK\r\n\r\nok".into())],
            "/health",
            true,
        ),
        (
            "503 not ready",
            vec![ok("503 Service Unavailable")],
            "/health",
            false,
        ),
        (
            "500",
            vec![ok("500 Internal Server Error")],
            "/health",
            false,
        ),
        ("404", vec![ok("404 Not Found")], "/health", false),
        (
            "300 without location",
            vec![ok("300 Multiple Choices")],
            "/health",
            false,
        ),
        (
            "302 without location",
            vec![ok("302 Found")],
            "/health",
            false,
        ),
        ("304", vec![ok("304 Not Modified")], "/health", false),
        (
            "305 with location",
            vec![redirect(305, "/x"), ok("200 OK")],
            "/health",
            false,
        ),
        (
            "101",
            vec![Reply::Bytes(
                "HTTP/1.1 101 Switching Protocols\r\n\r\n".into(),
            )],
            "/health",
            false,
        ),
        (
            "100 continue then 200",
            vec![Reply::Bytes(
                "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".into(),
            )],
            "/health",
            true,
        ),
        (
            "100 continue then 503",
            vec![Reply::Bytes(
                "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 503 No\r\nContent-Length: 0\r\n\r\n".into(),
            )],
            "/health",
            false,
        ),
        (
            "301 relative",
            vec![redirect(301, "/ready"), ok("200 OK")],
            "/health",
            true,
        ),
        (
            "302 relative then 503",
            vec![redirect(302, "ready"), ok("503 No")],
            "/health",
            false,
        ),
        (
            "303 absolute",
            vec![redirect(303, "http://127.0.0.1:{port}/x"), ok("200 OK")],
            "/health",
            true,
        ),
        (
            "307",
            vec![redirect(307, "/x"), ok("200 OK")],
            "/health",
            true,
        ),
        (
            "308",
            vec![redirect(308, "/x"), ok("200 OK")],
            "/health",
            true,
        ),
        (
            "URI header",
            vec![
                Reply::Bytes("HTTP/1.1 302 Found\r\nURI: /x\r\nContent-Length: 0\r\n\r\n".into()),
                ok("200 OK"),
            ],
            "/health",
            true,
        ),
        ("10 redirects", chain(10), "/health", true),
        ("11 redirects", chain(11), "/health", false),
        (
            "same URL 5 times",
            vec![redirect(302, "/loop")],
            "/health",
            false,
        ),
        (
            "https redirect",
            vec![redirect(302, "https://127.0.0.1:{port}/x")],
            "/health",
            false,
        ),
        (
            "file redirect",
            vec![redirect(302, "file:///etc/hostname")],
            "/health",
            false,
        ),
        ("closed at once", vec![Reply::Close], "/health", false),
        (
            "silent",
            vec![Reply::Silent(Duration::from_secs(3))],
            "/health",
            false,
        ),
        (
            "HTTP/2.0 status",
            vec![Reply::Bytes("HTTP/2.0 200 OK\r\n\r\n".into())],
            "/health",
            false,
        ),
        (
            "not HTTP",
            vec![Reply::Bytes("SSH-2.0-x\r\n\r\n".into())],
            "/health",
            false,
        ),
        (
            "two-digit status",
            vec![Reply::Bytes("HTTP/1.1 20 OK\r\n\r\n".into())],
            "/health",
            false,
        ),
        (
            "101 headers",
            vec![Reply::Bytes(format!(
                "HTTP/1.1 200 OK\r\n{many_headers}\r\n"
            ))],
            "/health",
            false,
        ),
        (
            "100 headers",
            vec![Reply::Bytes(format!(
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n{hundred_headers}\r\n"
            ))],
            "/health",
            true,
        ),
        (
            "header line too long",
            vec![Reply::Bytes(format!(
                "HTTP/1.1 200 OK\r\n{long_header}\r\n"
            ))],
            "/health",
            false,
        ),
    ]
}

fn oracle_python() -> Option<String> {
    match std::env::var("DOCLING_HEALTH_ORACLE_PYTHON") {
        Ok(python) if !python.is_empty() => Some(python),
        _ => {
            assert!(
                std::env::var("DOCLING_HEALTH_ORACLE_REQUIRED").as_deref() != Ok("1"),
                "DOCLING_HEALTH_ORACLE_REQUIRED=1 but DOCLING_HEALTH_ORACLE_PYTHON is unset"
            );
            None
        }
    }
}

fn reference_healthy(python: &str, url: &str, timeout: Duration) -> bool {
    std::process::Command::new(python)
        .args([
            "-I",
            "-c",
            "import sys, urllib.request; urllib.request.urlopen(sys.argv[1], timeout=float(sys.argv[2]))",
            url,
            &timeout.as_secs_f64().to_string(),
        ])
        .env_remove("http_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("https_proxy")
        .env_remove("HTTPS_PROXY")
        .env_remove("all_proxy")
        .env_remove("ALL_PROXY")
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

#[test]
fn scenarios_answer_as_expected() {
    for (name, script, path, healthy) in scenarios() {
        let server = serve(script);
        let url = format!("http://127.0.0.1:{}{}", server.port, path);
        let result = probe(&url, SHORT);
        assert_eq!(result.is_ok(), healthy, "{name}: {result:?}");
    }
}

#[test]
fn reference_agrees_on_every_scenario() {
    let Some(python) = oracle_python() else {
        eprintln!("skipped: DOCLING_HEALTH_ORACLE_PYTHON unset");
        return;
    };
    for (name, script, path, healthy) in scenarios() {
        let server = serve(script);
        let url = format!("http://127.0.0.1:{}{}", server.port, path);
        assert_eq!(
            reference_healthy(&python, &url, SHORT),
            healthy,
            "reference, {name}"
        );
    }
    let url = format!("http://127.0.0.1:{}/health", closed_port());
    assert!(
        !reference_healthy(&python, &url, SHORT),
        "reference, refused"
    );
}

#[test]
fn request_is_a_plain_get_with_host() {
    let server = serve(vec![redirect(302, "/next?x=1"), ok("200 OK")]);
    probe(&format!("http://127.0.0.1:{}/health", server.port), SHORT).unwrap();
    let requests = server.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].starts_with("GET /health HTTP/1.1\r\n"),
        "{}",
        requests[0]
    );
    assert!(requests[0].contains(&format!("\r\nHost: 127.0.0.1:{}\r\n", server.port)));
    assert!(requests[0].contains("\r\nConnection: close\r\n"));
    assert!(
        requests[1].starts_with("GET /next?x=1 HTTP/1.1\r\n"),
        "{}",
        requests[1]
    );
}

#[test]
fn refused_and_silent_fail_within_the_timeout() {
    let refused = probe(&format!("http://127.0.0.1:{}/health", closed_port()), SHORT);
    assert!(matches!(refused, Err(Unhealthy::Connect(_))), "{refused:?}");
    let server = serve(vec![Reply::Silent(Duration::from_secs(5))]);
    let started = Instant::now();
    let silent = probe(&format!("http://127.0.0.1:{}/health", server.port), SHORT);
    assert!(matches!(silent, Err(Unhealthy::Connect(_))), "{silent:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn failures_name_the_status() {
    let server = serve(vec![ok("503 Service Unavailable")]);
    let result = probe(&format!("http://127.0.0.1:{}/health", server.port), SHORT);
    assert_eq!(result, Err(Unhealthy::Status(503)));
    assert_eq!(result.unwrap_err().to_string(), "HTTP 503");
}
