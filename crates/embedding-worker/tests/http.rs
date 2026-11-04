#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use embedding_worker::{fill::Embed, http::Http, service::Loader, Control, Target};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
async fn stub(
    replies: Vec<(u16, String)>,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let captured = Arc::new(Mutex::new(vec![]));
    let output = captured.clone();
    let task = tokio::spawn(async move {
        for (status, body) in replies {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut request = vec![];
            let mut buf = [0; 8192];
            loop {
                let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
                    .await
                    .unwrap()
                    .unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let size = head
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= size {
                        break;
                    }
                }
            }
            output
                .lock()
                .unwrap()
                .push(String::from_utf8(request).unwrap());
            let response=format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (base, captured, task)
}
fn target(base: Option<String>) -> Target {
    Target {
        model: "synthetic".into(),
        profile: "nomic-v1".into(),
        base_url: base,
    }
}
#[tokio::test]
async fn loader_token_is_scoped_and_embedding_preserves_surrogates_and_index_order() {
    let (base, captured, task) = stub(vec![
        (200, r#"{"managed":true}"#.into()),
        (
            200,
            r#"{"data":[{"id":"synthetic","status":{"value":"loaded"}}]}"#.into(),
        ),
        (
            200,
            r#"{"data":[{"index":1,"embedding":[0,1]},{"index":0,"embedding":[1,0]}]}"#.into(),
        ),
    ])
    .await;
    let http = Http::new(
        &base,
        Some("synthetic-loader-token".into()),
        &base,
        Some("synthetic-inference-key".into()),
    )
    .unwrap();
    assert_eq!(http.get().await.unwrap().0, 200);
    let t = target(Some(base));
    assert_eq!(http.served(&t).await.unwrap(), vec!["synthetic"]);
    let vectors = http
        .embed(&t, &[vec![0xd800], vec![97]], &Control::default())
        .await
        .unwrap();
    assert_eq!(vectors, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    task.await.unwrap();
    let req = captured.lock().unwrap();
    assert!(req[0]
        .to_lowercase()
        .contains("x-model-loader-token: synthetic-loader-token"));
    for req in req.iter().skip(1) {
        assert!(!req.contains("synthetic-loader-token"));
        assert!(!req.contains("synthetic-inference-key"));
    }
    assert!(req[2].contains("\\ud800"));
    assert!(req[2].starts_with("POST /v1/embeddings"));
}
#[tokio::test]
async fn empty_batch_falls_back_to_singles_and_engine_auth_stays_at_fallback() {
    let (base, captured, task) = stub(vec![
        (200, r#"{"data":[]}"#.into()),
        (200, r#"{"data":[{"embedding":[1,0]}]}"#.into()),
        (200, r#"{"data":[{"embedding":[0,1]}]}"#.into()),
    ])
    .await;
    let http = Http::new(&base, None, &base, Some("synthetic-inference-key".into())).unwrap();
    let result = http
        .embed(&target(None), &[vec![97], vec![98]], &Control::default())
        .await
        .unwrap();
    assert_eq!(result.len(), 2);
    task.await.unwrap();
    assert!(captured.lock().unwrap().iter().all(|r| r
        .to_lowercase()
        .contains("authorization: bearer synthetic-inference-key")));
}
#[tokio::test]
async fn pairing_is_bound_to_start_and_unknown_core_keeps_native_idle() {
    let (base, _, task) = stub(vec![
        (
            200,
            r#"{"protocol":"rust-embedding-worker-v1","start":"wrong","nodeWorkersStopped":true}"#
                .into(),
        ),
        (
            200,
            r#"{"protocol":"rust-embedding-worker-v1","start":"boot","nodeWorkersStopped":true}"#
                .into(),
        ),
        (404, "{}".into()),
    ])
    .await;
    let http = Http::new(&base, None, &base, None).unwrap();
    assert!(http
        .ownership(&base, "synthetic-token", "boot")
        .await
        .is_err());
    assert!(http
        .ownership(&base, "synthetic-token", "boot")
        .await
        .is_ok());
    assert!(http
        .ownership(&base, "synthetic-token", "boot")
        .await
        .is_err());
    task.await.unwrap();
}
#[tokio::test]
async fn redirects_public_endpoints_and_url_credentials_are_refused() {
    let (base, _, task) = stub(vec![(302, "{}".into())]).await;
    let http = Http::new(&base, None, &base, None).unwrap();
    assert!(http.get().await.is_err());
    task.await.unwrap();
    assert!(Http::new("http://secret@localhost", None, &base, None).is_err());
    assert!(http
        .served(&target(Some("http://203.0.113.1".into())))
        .await
        .is_err());
}
#[tokio::test]
async fn embedding_admission_is_a_guard_only_lease_and_is_released() {
    let (engine, requests, engine_task) =
        stub(vec![(200, r#"{"data":[{"embedding":[1,0]}]}"#.into())]).await;
    let (core, core_requests, core_task) = stub(vec![
        (
            201,
            r#"{"lease":"0123456789abcdef0123456789abcdef","ttlMs":30000}"#.into(),
        ),
        (204, "null".into()),
    ])
    .await;
    let http = Http::new(&engine, None, &engine, None)
        .unwrap()
        .with_admission(&core, "synthetic-token")
        .unwrap();
    http.embed(&target(Some(engine)), &[vec![97]], &Control::default())
        .await
        .unwrap();
    engine_task.await.unwrap();
    core_task.await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
    let core = core_requests.lock().unwrap();
    assert!(core[0].contains("\"embedding\":true"));
    assert!(core[1].starts_with("DELETE /internal/inference-admission/lease/"));
}
