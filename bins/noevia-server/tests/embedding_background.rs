//! Synthetic app construction and real lifecycle: no production or model access.
#![allow(clippy::unwrap_used)]
#[path = "mcp_support/mod.rs"]
mod support;
use support::*;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_loader_starts_nothing_and_capability_is_explicit() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let front = front_env(dir.path(), true, &[]).await;
    assert!(
        noevia_server::embedding_background::start(&front.app, |_| None)
            .unwrap()
            .is_none()
    );
    assert!(!dir.path().join("embedding-worker.lock").exists());
    assert!(noevia_server::FEATURES.contains(&"rust-embedding-background-v1"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_core_cannot_start_native_work_and_shutdown_releases_the_owner_lock() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let up = upstream().await;
    up.set(|_| reply("404 Not Found", "{}"));
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", up.base()),
            ("NOEVIA_PROJECT_STORE_PORT", "19021".into()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
            ("NOEVIA_START_ID", "11111111111111111111111111111111".into()),
        ],
    )
    .await;
    let lookup = |key: &str| match key {
        "MODEL_LOADER_URL" => Some(up.base()),
        _ => None,
    };
    let worker = noevia_server::embedding_background::start(&front.app, lookup)
        .unwrap()
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    assert!(noevia_server::embedding_background::start(&front.app, lookup).is_err());
    tokio::time::timeout(std::time::Duration::from_secs(8), worker.shutdown())
        .await
        .unwrap();
    assert!(up
        .seen()
        .iter()
        .all(|r| r.path == "/internal/embedding-worker-owner"));
    assert!(!dir.path().join("embedding-worker-status.json").exists());
    let again = noevia_server::embedding_background::start(&front.app, lookup)
        .unwrap()
        .unwrap();
    again.shutdown().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paired_owner_refreshes_at_ten_seconds_and_kicks_saved_next_at_thirty() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let up = upstream().await;
    up.set(|r|match r.path.as_str() {
        "/internal/embedding-worker-owner"=>reply("200 OK",r#"{"protocol":"rust-embedding-worker-v1","start":"11111111111111111111111111111111","nodeWorkersStopped":true}"#),
        "/api/v1/embedding-service"=>reply("200 OK",r#"{"managed":true,"enabled":true,"models":[],"previous":[],"running":{"loaded":[]}}"#),
        "/v1/models"=>reply("200 OK",r#"{"data":[{"id":"next","status":{"value":"loaded"}}]}"#),
        _=>reply("500 Unexpected","{}"),
    });
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", up.base()),
            ("NOEVIA_PROJECT_STORE_PORT", "19021".into()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
            ("NOEVIA_START_ID", "11111111111111111111111111111111".into()),
        ],
    )
    .await;
    let lookup = |key: &str| match key {
        "MODEL_LOADER_URL" | "INFERENCE_BASE_URL" => Some(up.base()),
        "EMBEDDING_NEXT_MODEL" => Some("next".into()),
        _ => None,
    };
    let begun = tokio::time::Instant::now();
    let worker = noevia_server::embedding_background::start(&front.app, lookup)
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(38), async {
        loop {
            if std::fs::read(dir.path().join("embedding-worker-status.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .is_some_and(|v| v.get("lastPass").is_some_and(|p| !p.is_null()))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap();
    assert!(begun.elapsed() >= std::time::Duration::from_secs(30));
    let seen = up.seen();
    let refreshes = seen
        .iter()
        .filter(|r| r.path == "/api/v1/embedding-service")
        .count();
    assert!((3..=4).contains(&refreshes));
    assert!(seen.iter().any(|r| r.path == "/v1/models"));
    assert!(!seen.iter().any(|r| r.path == "/v1/embeddings"));
    tokio::time::timeout(std::time::Duration::from_secs(8), worker.shutdown())
        .await
        .unwrap();
}
