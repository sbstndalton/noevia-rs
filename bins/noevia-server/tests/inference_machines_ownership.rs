//! The same behavioral regression test is run on the assigned base and the completed port.
#![allow(clippy::unwrap_used)]
#[path = "mcp_support/mod.rs"]
mod support;
use support::*;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_is_answered_by_native_owner_without_public_node_fallback() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path());
    let node = upstream().await;
    node.set(|r| {
        if r.path == "/internal/inference-machines/probes" {
            reply("200 OK", &j(&r.body).get("view").unwrap().to_string())
        } else {
            reply("418 I'm a teapot", r#"{"publicNodeFallback":true}"#)
        }
    });
    let front = front_env(
        dir.path(),
        true,
        &[
            ("NOEVIA_LEGACY_UPSTREAM", node.base()),
            ("NOEVIA_PROJECT_STORE_TOKEN", STORE_TOKEN.into()),
        ],
    )
    .await;
    assert_eq!(get(front.addr, "/api/models/machines", ADMIN).await.0, 200);
    assert_eq!(node.count("/api/models/machines"), 0);
}
