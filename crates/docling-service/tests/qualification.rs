#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{path::PathBuf, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[path = "support/oracle.rs"]
mod oracle;
#[tokio::test]
async fn actual_python_http_goldens_through_native_real_worker() {
    let source = PathBuf::from(
        std::env::var("NOEVIA_DOCLING_REFERENCE")
            .expect("required pinned Docling source; see README"),
    );
    let directory = tempfile::tempdir().unwrap();
    oracle::prepare(&source, directory.path()).unwrap();
    let mut config = docling_service::Config::production(directory.path().to_owned());
    config.python = "python3".into();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(docling_service::serve(listener, config));
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/python-http.v1.json")).unwrap();
    let mut count = 0;
    for case in fixture["cases"].as_array().unwrap() {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let mut header = format!(
            "{} {} HTTP/1.0\r\nHost: localhost\r\n",
            case["method"].as_str().unwrap(),
            case["path"].as_str().unwrap()
        );
        if let Some(length) = case["length"].as_str() {
            header.push_str(&format!("Content-Length: {length}\r\n"));
        }
        if let Some(name) = case["name"].as_str() {
            header.push_str(&format!("X-Document-Name: {name}\r\n"));
        }
        let mut data = header.into_bytes();
        data.extend_from_slice(b"\r\n");
        data.extend_from_slice(&STANDARD.decode(case["data"].as_str().unwrap()).unwrap());
        socket.write_all(&data).await.unwrap();
        let mut response = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(5), socket.read_to_end(&mut response))
            .await
            .unwrap();
        assert!(
            read.is_ok() || read.is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionReset)
        );
        let split = response.windows(4).position(|b| b == b"\r\n\r\n").unwrap();
        let headers = String::from_utf8_lossy(&response[..split]);
        let size = headers
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .trim()
            .parse::<usize>()
            .unwrap();
        assert_eq!(
            response.len() - split - 4,
            if case["method"] == "HEAD" { 0 } else { size },
            "complete HTTP reply required"
        );
        let status = headers
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let content_type = headers
            .lines()
            .find_map(|line| line.strip_prefix("Content-Type: "))
            .unwrap()
            .trim();
        let body: serde_json::Value = if content_type == "application/json" {
            serde_json::from_slice(&response[split + 4..]).unwrap()
        } else {
            serde_json::Value::String(String::from_utf8(response[split + 4..].to_vec()).unwrap())
        };
        assert_eq!(
            serde_json::json!({"status":status,"type":content_type,"body":body}),
            case["expected"],
            "case {count}"
        );
        count += 1;
    }
    assert_eq!(count, 34, "no corpus rows silently dropped");
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn real_worker_disconnect_kills_group_and_cleans_generated_input() {
    let source = PathBuf::from(
        std::env::var("NOEVIA_DOCLING_REFERENCE").expect("required pinned Docling source"),
    );
    let directory = tempfile::tempdir().unwrap();
    std::fs::copy(
        source.join("isolated_worker.py"),
        directory.path().join("isolated_worker.py"),
    )
    .unwrap();
    let notification = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let notify_port = notification.local_addr().unwrap().port();
    // The only replaced component is the engine. Its fixed test child command
    // has a short independent self-exit bound even when a cleanup mutation fails.
    std::fs::write(directory.path().join("extract.py"),format!(r#"
import json,os,socket,subprocess,sys,time
def extract(path,name):
 if open(path,'rb').read()==b'FORK':
  child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(3)'])
  socket.socket(socket.AF_INET,socket.SOCK_DGRAM).sendto(json.dumps({{'worker':os.getpid(),'child':child.pid,'path':path}}).encode(),('127.0.0.1',{notify_port}))
  time.sleep(30)
 return {{'text':'synthetic'}}
"#)).unwrap();
    let mut config = docling_service::Config::production(directory.path().to_owned());
    config.python = "python3".into();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(docling_service::serve(listener, config));
    for half in [false, true] {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        socket.write_all(b"POST /extract HTTP/1.0\r\nHost: localhost\r\nX-Document-Name: x.md\r\nContent-Length: 4\r\n\r\nFORK").await.unwrap();
        let mut buffer = [0u8; 4096];
        let count = tokio::time::timeout(Duration::from_secs(2), notification.recv(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        let started: serde_json::Value = serde_json::from_slice(&buffer[..count]).unwrap();
        let input = PathBuf::from(started["path"].as_str().unwrap());
        assert!(input.is_file());
        assert_eq!(input.file_name().unwrap(), "input.md");
        assert!(input
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("noevia-docling-"));
        if half {
            socket.shutdown().await.unwrap();
            let mut bytes = Vec::new();
            tokio::time::timeout(Duration::from_secs(1), socket.read_to_end(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            assert!(bytes.is_empty());
        } else {
            drop(socket);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            !input.parent().unwrap().exists(),
            "generated temporary directory survived cancellation"
        );
        let worker = i32::try_from(started["worker"].as_i64().unwrap()).unwrap();
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(worker), None).is_err(),
            "worker not reaped"
        );
        let child = started["child"].as_i64().unwrap();
        let status = std::process::Command::new("ps")
            .args(["-p", &child.to_string(), "-o", "stat="])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&status.stdout);
        assert!(
            state.trim().is_empty() || state.trim().starts_with('Z'),
            "engine descendant remains running"
        );
    }
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn unsupported_methods_and_head_match_production_python312() {
    let source = PathBuf::from(
        std::env::var("NOEVIA_DOCLING_REFERENCE").expect("required pinned Docling source"),
    );
    let directory = tempfile::tempdir().unwrap();
    oracle::prepare(&source, directory.path()).unwrap();
    let reference = oracle::Server::start(directory.path()).unwrap();
    let production = reference.production;
    if std::env::var("CI").is_ok() {
        assert!(
            production,
            "mandatory Linux oracle must use production Python3.12"
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(docling_service::serve(
        listener,
        docling_service::Config::production(directory.path().to_owned()),
    ));
    for method in ["PUT", "DELETE", "HEAD"] {
        let expected =
            oracle::exchange(reference.port, method, "/extract", None, None, b"").unwrap();
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        socket
            .write_all(format!("{method} /extract HTTP/1.0\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), socket.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        let split = bytes.windows(4).position(|b| b == b"\r\n\r\n").unwrap();
        let headers = String::from_utf8_lossy(&bytes[..split]);
        assert!(headers.starts_with("HTTP/1.0 501"));
        assert!(headers.contains("Content-Type: text/html;charset=utf-8"));
        let size = headers
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .trim()
            .parse::<usize>()
            .unwrap();
        assert_eq!(
            bytes.len() - split - 4,
            if method == "HEAD" { 0 } else { size }
        );
        let body = String::from_utf8(bytes[split + 4..].to_vec()).unwrap();
        if production {
            assert_eq!(body, expected.get("body").as_str().unwrap());
        } else if method != "HEAD" {
            assert!(body.contains(&format!("Unsupported method ('{method}')")));
        }
    }
    // Non-3.12 local Python proves status/type/HEAD only; full HTML parity is
    // mandatory under CI's separately asserted production interpreter.
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn real_worker_maximum_unicode_text_and_page_envelope_fit_frame_cap() {
    let source = PathBuf::from(
        std::env::var("NOEVIA_DOCLING_REFERENCE").expect("required pinned Docling source"),
    );
    let root = tempfile::tempdir().unwrap();
    std::fs::copy(
        source.join("isolated_worker.py"),
        root.path().join("isolated_worker.py"),
    )
    .unwrap();
    std::fs::write(root.path().join("extract.py"),"def extract(path,name):\n return {'pages':[{'number':n,'text':'\\U0001f4c4'*200000 if n<=10 else '', 'status':'native','method':'docling','truncated':False} for n in range(1,301)],'total':300,'truncatedPages':False}\n").unwrap();
    let mut config = docling_service::Config::production(root.path().to_owned());
    config.python = "python3".into();
    let path = root.path().join("input.md");
    std::fs::write(&path, "synthetic").unwrap();
    let mut worker = docling_service::Worker::new(config);
    let result = worker
        .convert(
            &path,
            "x.md",
            tokio::time::Instant::now() + Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert_eq!(result["pages"].as_array().unwrap().len(), 300);
    assert_eq!(
        result["pages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["text"].as_str().unwrap().chars().count())
            .sum::<usize>(),
        2_000_000
    );
    worker.stop().await;
}

#[test]
fn rust_oracle_reproduces_fixture_and_refuses_changed_source() {
    let source = PathBuf::from(
        std::env::var("NOEVIA_DOCLING_REFERENCE").expect("required pinned Docling source"),
    );
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("fixture.json");
    oracle::generate(&source, &output).unwrap();
    assert_eq!(
        std::fs::read(&output).unwrap(),
        include_bytes!("fixtures/python-http.v1.json")
    );
    let copied = directory.path().join("changed-source");
    std::fs::create_dir(&copied).unwrap();
    for (name, _) in oracle::HASHES {
        std::fs::copy(source.join(name), copied.join(name)).unwrap();
    }
    std::fs::write(copied.join("server.py"), "changed source").unwrap();
    let target = directory.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let error = oracle::prepare(&copied, &target).unwrap_err();
    assert!(error
        .to_string()
        .contains("source hash mismatch: server.py"));
    assert_eq!(
        std::fs::read_dir(target).unwrap().count(),
        0,
        "source validation must precede copies/imports"
    );
}
