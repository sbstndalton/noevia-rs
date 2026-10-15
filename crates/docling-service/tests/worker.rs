#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use docling_service::{Config, Failure, Worker};
use std::time::Duration;
use tokio::time::Instant;

fn fixture() -> (tempfile::TempDir, Config) {
    let directory = tempfile::tempdir().unwrap();
    // Test collaborator implements the production protocol without importing models.
    std::fs::write(directory.path().join("isolated_worker.py"), r#"
import json,os,struct,sys,time
def send(v):
 b=json.dumps(v).encode();sys.stdout.buffer.write(struct.pack('>I',len(b))+b);sys.stdout.buffer.flush()
send({'ready':True})
for line in sys.stdin:
 r=json.loads(line);d=open(r['path'],'rb').read()
 if d==b'sleep':time.sleep(30)
 elif d==b'bad':sys.stdout.buffer.write(struct.pack('>I',40000000));sys.stdout.buffer.flush()
 elif d==b'error':send({'error':True})
 elif d==b'echo':send({'ok':{'request':r}})
 elif d[:5]==b'large':send({'ok':{'request':r,'size':len(d)}})
 else:send({'ok':{'pid':os.getpid(),'text':'synthetic'}})
"#).unwrap();
    let mut config = Config::production(directory.path().to_owned());
    config.python = "python3".into();
    config.ready_timeout = Duration::from_secs(2);
    (directory, config)
}
#[tokio::test]
async fn reuse_errors_timeout_and_oversized_frame() {
    let (directory, config) = fixture();
    let path = directory.path().join("input.docx");
    let mut worker = Worker::new(config);
    std::fs::write(&path, "ok").unwrap();
    let first = worker
        .convert(&path, "x.docx", Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
    std::fs::write(&path, "error").unwrap();
    assert_eq!(
        worker
            .convert(&path, "x.docx", Instant::now() + Duration::from_secs(2))
            .await,
        Err(Failure::Conversion)
    );
    std::fs::write(&path, "ok").unwrap();
    assert_eq!(
        worker
            .convert(&path, "x.docx", Instant::now() + Duration::from_secs(2))
            .await
            .unwrap(),
        first
    );
    std::fs::write(&path, "sleep").unwrap();
    assert_eq!(
        worker
            .convert(&path, "x.docx", Instant::now() + Duration::from_millis(50))
            .await,
        Err(Failure::Timeout)
    );
    std::fs::write(&path, "ok").unwrap();
    assert_ne!(
        worker
            .convert(&path, "x.docx", Instant::now() + Duration::from_secs(2))
            .await
            .unwrap()["pid"],
        first["pid"]
    );
    std::fs::write(&path, "bad").unwrap();
    assert_eq!(
        worker
            .convert(&path, "x.docx", Instant::now() + Duration::from_secs(2))
            .await,
        Err(Failure::Conversion)
    );
    worker.stop().await;
}
#[tokio::test]
async fn import_failure_is_unavailable() {
    let (directory, config) = fixture();
    std::fs::write(
        directory.path().join("isolated_worker.py"),
        "raise RuntimeError('synthetic')",
    )
    .unwrap();
    let mut worker = Worker::new(config);
    assert_eq!(
        worker
            .convert(
                &directory.path().join("input"),
                "x.docx",
                Instant::now() + Duration::from_secs(2)
            )
            .await,
        Err(Failure::Unavailable)
    );
}

#[tokio::test]
async fn bad_or_slow_readiness_is_unavailable_and_next_request_recovers() {
    for script in ["import time;time.sleep(30)","import sys,struct;sys.stdout.buffer.write(struct.pack('>I',30));sys.stdout.buffer.flush()","import json,sys,struct;b=json.dumps({'ready':True,'extra':True}).encode();sys.stdout.buffer.write(struct.pack('>I',len(b))+b);sys.stdout.buffer.flush()"] {
        let (root,mut config)=fixture();
        config.ready_timeout=Duration::from_millis(50);
        let worker_source=std::fs::read(root.path().join("isolated_worker.py")).unwrap();
        std::fs::write(root.path().join("isolated_worker.py"),script).unwrap();
        let path=root.path().join("input.docx");std::fs::write(&path,"ok").unwrap();
        let mut worker=Worker::new(config);
        assert_eq!(worker.convert(&path,"x.docx",Instant::now()+Duration::from_secs(1)).await,Err(Failure::Unavailable));
        std::fs::write(root.path().join("isolated_worker.py"),worker_source).unwrap();
        assert!(worker.convert(&path,"x.docx",Instant::now()+Duration::from_secs(1)).await.is_ok());
        worker.stop().await;
    }
}

#[tokio::test]
async fn absolute_document_deadline_includes_readiness() {
    let (root, mut config) = fixture();
    config.ready_timeout = Duration::from_secs(1);
    std::fs::write(
        root.path().join("isolated_worker.py"),
        "import time;time.sleep(30)",
    )
    .unwrap();
    let mut worker = Worker::new(config);
    assert_eq!(
        worker
            .convert(
                &root.path().join("input.docx"),
                "x.docx",
                Instant::now() + Duration::from_millis(30)
            )
            .await,
        Err(Failure::Timeout)
    );
}

async fn request(port: u16, name: &str, body: &[u8]) -> (u16, serde_json::Value) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    socket.write_all(format!("POST /extract HTTP/1.0\r\nHost: localhost\r\nX-Document-Name: {name}\r\nContent-Length: {}\r\n\r\n",body.len()).as_bytes()).await.unwrap();
    socket.write_all(body).await.unwrap();
    let mut bytes = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut bytes))
        .await
        .unwrap();
    // Early refusal does not drain an upload (same Python contract). A reset is
    // acceptable only AFTER an exact complete HTTP response has been received.
    assert!(
        read.is_ok()
            || read
                .as_ref()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::ConnectionReset)
    );
    let split = bytes.windows(4).position(|b| b == b"\r\n\r\n").unwrap();
    let header = String::from_utf8_lossy(&bytes[..split]);
    let length = header
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .unwrap()
        .trim()
        .parse::<usize>()
        .unwrap();
    assert_eq!(bytes.len() - split - 4, length);
    let status = String::from_utf8_lossy(&bytes[..split])
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, serde_json::from_slice(&bytes[split + 4..]).unwrap())
}
#[tokio::test]
async fn raw_http_full_and_half_close_cancel_and_release_slot() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (_directory, mut config) = fixture();
    config.short_timeout = Duration::from_secs(2);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(docling_service::serve(listener, config));
    assert_eq!(request(port, "x.docx", b"ok").await.0, 200);
    for half in [false, true] {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        socket.write_all(b"POST /extract HTTP/1.0\r\nHost: localhost\r\nX-Document-Name: x.docx\r\nContent-Length: 5\r\n\r\nsleep").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // A subsequent request observing 503 proves the first owns its conversion
        // slot before cancellation. Bounded scheduler yield, not elapsed-time guess.
        let busy = request(port, "x.docx", b"ok").await;
        assert_eq!(busy.0, 503);
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
        }
        // EOF cancellation and reap complete before the socket is closed; a full
        // close has no acknowledgment, so one bounded wait precedes its snapshot.
        if !half {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(request(port, "next.docx", b"ok").await.0, 200);
    }
    assert_eq!(request(port, "a/b.docx", b"ok").await.0, 400);
    assert_eq!(request(port, "x.zip", b"ok").await.0, 415);
    assert_eq!(request(port, "x.docx", b"error").await.0, 422);
    server.abort();
}

#[tokio::test]
async fn slow_incomplete_body_and_connection_admission_are_bounded() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (_directory, mut config) = fixture();
    config.body_timeout = Duration::from_millis(50);
    config.header_timeout = Duration::from_secs(2);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(docling_service::serve(listener, config));
    let mut incomplete = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    incomplete.write_all(b"POST /extract HTTP/1.0\r\nHost: localhost\r\nX-Document-Name: x.docx\r\nContent-Length: 2\r\n\r\nx").await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), incomplete.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(bytes.starts_with(b"HTTP/1.0 400"));
    assert!(String::from_utf8_lossy(&bytes).contains("Incomplete document body."));
    assert_eq!(request(port, "x.docx", b"ok").await.0, 200);
    let mut stalled = Vec::new();
    for _ in 0..docling_service::CONNECTION_CAP {
        stalled.push(
            tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap(),
        );
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut overflow = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), overflow.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0,
        "unbounded queued connection accepted"
    );
    drop(stalled);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(request(port, "x.docx", b"ok").await.0, 200);
    server.abort();
}

// sbstndalton/noevia#1726: the figures opt-in reaches the engine; without it the request is unchanged.
#[tokio::test]
async fn figures_are_forwarded_only_when_asked() {
    let (directory, config) = fixture();
    let path = directory.path().join("input.pdf");
    std::fs::write(&path, "echo").unwrap();
    let mut worker = Worker::new(config);
    let plain = worker
        .convert(&path, "x.pdf", Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
    let keys: Vec<&String> = plain["request"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["name", "path"]);
    let with = worker
        .convert_with(
            &path,
            "x.pdf",
            true,
            Instant::now() + Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert_eq!(with["request"]["figures"], true);
    assert_eq!(with["request"]["name"], "x.pdf");
}

#[tokio::test]
async fn ranges_are_forwarded_and_a_ranged_body_over_25_mib_is_streamed() {
    // sbstndalton/noevia#1867: X-Document-Pages reaches the engine as "pages": [first, last], and a
    // ranged PDF may exceed the 25 MiB whole-document cap (it is written to the temp file in chunks).
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (directory, config) = fixture();
    let path = directory.path().join("input.pdf");
    std::fs::write(&path, "echo").unwrap();
    let mut worker = Worker::new(config.clone());
    let ranged = worker
        .convert_range(
            &path,
            "x.pdf",
            false,
            Some((26, 50)),
            Instant::now() + Duration::from_secs(2),
        )
        .await
        .unwrap();
    assert_eq!(ranged["request"]["pages"], serde_json::json!([26, 50]));
    assert!(ranged["request"].get("figures").is_none());
    worker.stop().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(docling_service::serve(listener, config));
    let size = docling_service::BODY_CAP + 5 * 1024 * 1024;
    let mut body = vec![0u8; size];
    body[..5].copy_from_slice(b"large");
    for (pages, status) in [(None, "400"), (Some("2-3"), "200")] {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let mut head = format!("POST /extract HTTP/1.0\r\nHost: localhost\r\nX-Document-Name: big.pdf\r\nContent-Length: {size}\r\n");
        if let Some(p) = pages {
            head.push_str(&format!("X-Document-Pages: {p}\r\n"));
        }
        head.push_str("\r\n");
        socket.write_all(head.as_bytes()).await.unwrap();
        if pages.is_some() {
            socket.write_all(&body).await.unwrap();
        }
        let mut reply = Vec::new();
        tokio::time::timeout(Duration::from_secs(20), socket.read_to_end(&mut reply))
            .await
            .unwrap()
            .unwrap();
        let text = String::from_utf8_lossy(&reply);
        assert!(text.starts_with(&format!("HTTP/1.0 {status}")), "{text}");
        if pages.is_some() {
            let split = reply.windows(4).position(|b| b == b"\r\n\r\n").unwrap();
            let json: serde_json::Value = serde_json::from_slice(&reply[split + 4..]).unwrap();
            assert_eq!(json["size"], size, "received whole");
            assert_eq!(json["request"]["pages"], serde_json::json!([2, 3]));
        }
    }
    server.abort();
}
