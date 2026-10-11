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
    let path = directory.path().join("input.md");
    let mut worker = Worker::new(config);
    std::fs::write(&path, "ok").unwrap();
    let first = worker
        .convert(&path, "x.md", Instant::now() + Duration::from_secs(2))
        .await
        .unwrap();
    std::fs::write(&path, "error").unwrap();
    assert_eq!(
        worker
            .convert(&path, "x.md", Instant::now() + Duration::from_secs(2))
            .await,
        Err(Failure::Conversion)
    );
    std::fs::write(&path, "ok").unwrap();
    assert_eq!(
        worker
            .convert(&path, "x.md", Instant::now() + Duration::from_secs(2))
            .await
            .unwrap(),
        first
    );
    std::fs::write(&path, "sleep").unwrap();
    assert_eq!(
        worker
            .convert(&path, "x.md", Instant::now() + Duration::from_millis(50))
            .await,
        Err(Failure::Timeout)
    );
    std::fs::write(&path, "ok").unwrap();
    assert_ne!(
        worker
            .convert(&path, "x.md", Instant::now() + Duration::from_secs(2))
            .await
            .unwrap()["pid"],
        first["pid"]
    );
    std::fs::write(&path, "bad").unwrap();
    assert_eq!(
        worker
            .convert(&path, "x.md", Instant::now() + Duration::from_secs(2))
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
                "x.md",
                Instant::now() + Duration::from_secs(2)
            )
            .await,
        Err(Failure::Unavailable)
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
    assert_eq!(request(port, "x.md", b"ok").await.0, 200);
    for half in [false, true] {
        let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        socket.write_all(b"POST /extract HTTP/1.0\r\nHost: localhost\r\nX-Document-Name: x.md\r\nContent-Length: 5\r\n\r\nsleep").await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // A subsequent request observing 503 proves the first owns its conversion
        // slot before cancellation. Bounded scheduler yield, not elapsed-time guess.
        let busy = request(port, "x.md", b"ok").await;
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
        assert_eq!(request(port, "next.md", b"ok").await.0, 200);
    }
    assert_eq!(request(port, "a/b.md", b"ok").await.0, 400);
    assert_eq!(request(port, "x.zip", b"ok").await.0, 415);
    assert_eq!(request(port, "x.md", b"error").await.0, 422);
    server.abort();
}
