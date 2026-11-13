//! A native foreground read of this process (`embedding_worker::foreground`, held by the front's
//! own project search) makes the outbox and media passes wait exactly as Node's
//! `foregroundBusy: true` does. Its own test binary: the counter is process-wide.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod support;
use embedding_worker::{foreground, http::Http};

#[tokio::test]
async fn a_native_search_holds_the_passes_like_a_node_one() {
    let stub = support::server(|path, _| {
        if path == "/internal/embedding-worker-owner" {
            let mut proof: serde_json::Value = serde_json::from_str(support::PROOF).unwrap();
            proof["indexOutboxStopped"] = true.into();
            proof["mediaWorkersStopped"] = true.into();
            proof["foregroundBusy"] = false.into();
            return (200, proof.to_string());
        }
        support::default_reply(path)
    })
    .await;
    let http = Http::new("", None, &stub.base, None).unwrap();
    let ready = |tail: bool| {
        let http = http.clone();
        let base = stub.base.clone();
        async move {
            (
                http.outbox_ready(&base, "synthetic", support::START)
                    .await
                    .unwrap(),
                http.media_ready(&base, "synthetic", support::START, tail)
                    .await
                    .unwrap(),
            )
        }
    };
    assert_eq!(ready(true).await, (true, true));
    let search = foreground::enter();
    assert_eq!(ready(true).await, (false, false));
    assert_eq!(
        ready(false).await,
        (false, true),
        "figure extraction does not yield to chat"
    );
    drop(search);
    assert_eq!(ready(true).await, (true, true));
}
