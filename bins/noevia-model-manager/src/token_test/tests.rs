#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
#[test]
fn ordinary_response_shapes_and_secret_redaction() {
    for (status, text, expected) in [
        (
            200,
            r#"{"name":"synthetic-user","fullname":"Full Name"}"#,
            r#"{"message":"authenticated as synthetic-user","ok":true}"#,
        ),
        (
            200,
            r#"{"name":"","fullname":"Full Name"}"#,
            r#"{"message":"authenticated as Full Name","ok":true}"#,
        ),
        (
            200,
            "{}",
            r#"{"message":"authenticated as unknown","ok":true}"#,
        ),
        (
            401,
            "invalid token",
            r#"{"message":"HTTP 401: invalid token","ok":false}"#,
        ),
        (
            401,
            "synthetic-secret echoed",
            r#"{"message":"HTTP 401: [redacted] echoed","ok":false}"#,
        ),
    ] {
        assert_eq!(
            String::from_utf8(render(status, text.as_bytes(), "synthetic-secret").unwrap())
                .unwrap(),
            expected
        );
    }
    let boundary = format!("{}synthetic-secret echoed", "x".repeat(115));
    let redacted: serde_json::Value =
        serde_json::from_slice(&render(401, boundary.as_bytes(), "synthetic-secret").unwrap())
            .unwrap();
    assert_eq!(
        redacted["message"],
        format!("HTTP 401: {}[reda", "x".repeat(115))
    );
    assert!(!redacted["message"].as_str().unwrap().contains("synt"));
    assert!(render(200, b"not JSON", "secret").is_none());
    assert!(render(200, b"[]", "secret").is_none());
    let text = "é".repeat(121);
    let got: serde_json::Value =
        serde_json::from_slice(&render(403, text.as_bytes(), "secret").unwrap()).unwrap();
    assert_eq!(got["message"], format!("HTTP 403: {}", "é".repeat(120)));
}
#[tokio::test]
async fn no_token_needs_no_network_and_native_failures_are_local() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model_loader.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE kv(key TEXT PRIMARY KEY,value TEXT NOT NULL)")
        .unwrap();
    let tester = TokenTest::with("http://127.0.0.1:9/whoami-v2").unwrap();
    assert_eq!(
        tester.run(&path).await,
        answer(true, "anonymous (no token)")
    );
    db.execute("INSERT INTO kv VALUES('hf_token',?)", ["synthetic-secret"])
        .unwrap();
    assert_eq!(tester.run(&path).await, unavailable());
    db.execute("UPDATE kv SET value=?", ["é-secret"]).unwrap();
    assert_eq!(tester.run(&path).await, unavailable());
    assert_eq!(
        tester.run(&dir.path().join("missing.db")).await,
        unavailable()
    );
    assert!(!dir.path().join("missing.db").exists());
}

#[tokio::test]
async fn actual_python_oracle_over_real_local_http_and_bounded_failures() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn serve(
        status: u16,
        content: &str,
        body: Vec<u8>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let content = content.to_owned();
        let task = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let n = peer.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..n]);
            assert!(request.starts_with("GET /whoami-v2 HTTP/1.1"));
            assert!(request
                .to_ascii_lowercase()
                .contains("authorization: bearer synthetic-token"));
            let header = format!("HTTP/1.1 {status} QA\r\nContent-Type: {content}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            peer.write_all(header.as_bytes()).await.unwrap();
            // Oversized responses may be refused before their final write completes.
            let _ = peer.write_all(&body).await;
        });
        (format!("http://{address}/whoami-v2"), task)
    }
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/token-test-python.v1.json"
    ))
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model_loader.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE kv(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO kv VALUES('hf_token','synthetic-token')").unwrap();
    for case in fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["id"] != "empty")
    {
        let (endpoint, peer) = serve(
            case["status"].as_u64().unwrap() as u16,
            "application/json; charset=utf-8",
            case["body"].as_str().unwrap().as_bytes().to_vec(),
        )
        .await;
        let tester = TokenTest::with(&endpoint).unwrap();
        let got: serde_json::Value = serde_json::from_slice(&tester.run(&path).await).unwrap();
        assert_eq!(got, case["expected"], "{}", case["id"]);
        peer.await.unwrap();
    }
    let boundary = format!("{}synthetic-token echoed", "x".repeat(115));
    let (endpoint, peer) = serve(401, "text/plain", boundary.into_bytes()).await;
    let result = TokenTest::with(&endpoint).unwrap().run(&path).await;
    let result: serde_json::Value = serde_json::from_slice(&result).unwrap();
    assert_eq!(
        result["message"],
        format!("HTTP 401: {}[reda", "x".repeat(115))
    );
    peer.await.unwrap();
    for (content, body) in [
        ("application/json", vec![b'a'; CAP + 1]),
        ("application/json", b"not json".to_vec()),
        ("text/plain; charset=iso-8859-1", b"refused".to_vec()),
    ] {
        let (endpoint, peer) = serve(200, content, body).await;
        assert_eq!(
            TokenTest::with(&endpoint).unwrap().run(&path).await,
            unavailable()
        );
        peer.await.unwrap();
    }
}
