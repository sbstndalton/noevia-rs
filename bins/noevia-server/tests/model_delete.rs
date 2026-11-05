//! Synthetic router transport/retry oracle; public lifecycle policy is in model_mutations.rs.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
#[path = "mcp_support/mod.rs"]
mod support;

use noevia_server::model_maint::router::Router;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use support::*;

#[tokio::test]
async fn unload_then_delete_encodes_name_and_preserves_authority_headers() {
    let up = upstream().await;
    up.set(|r| {
        if r.path == "/models/unload" {
            reply("200 OK", "{}")
        } else {
            reply("200 OK", r#"{"deleted":["synthetic.gguf"]}"#)
        }
    });
    let router = Router::new(&up.base(), Some("synthetic-key")).unwrap();
    let name = "folder/é +?#&=!.gguf";
    let removed = router
        .remove_model(name, Some("synthetic-hold"))
        .await
        .unwrap();
    assert!(removed.unloaded);
    assert!(removed.reply.ok);
    assert_eq!(
        removed.reply.body,
        Some(json!({"deleted":["synthetic.gguf"]}))
    );
    let calls = up.seen();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        (calls[0].method.as_str(), calls[0].path.as_str()),
        ("POST", "/models/unload")
    );
    assert_eq!(j(&calls[0].body), json!({"model": name}));
    assert_eq!(
        (calls[1].method.as_str(), calls[1].path.as_str()),
        (
            "DELETE",
            "/models?model=folder%2F%C3%A9%20%2B%3F%23%26%3D!.gguf"
        )
    );
    assert!(calls[1].body.is_empty());
    for call in calls {
        assert_eq!(
            call.header("authorization").as_deref(),
            Some("bearer synthetic-key")
        );
        assert_eq!(
            call.header("x-noevia-maintenance").as_deref(),
            Some("synthetic-hold")
        );
        assert_eq!(call.header("x-noevia-gate-admin"), None);
    }
}

#[tokio::test]
async fn retries_every_settled_delete_refusal_exactly_once_and_keeps_last_reply() {
    for status in ["404 Not Found", "409 Conflict", "500 Failure"] {
        let up = upstream().await;
        up.set(move |r| if r.method == "DELETE" {
            reply(status, r#"{"error":{"type":"gate_error","code":"model_in_use","retryable":true,"message":"Busy"}}"#)
        } else { reply("404 Not Found", "{}") });
        let router = Router::new(&up.base(), None).unwrap();
        let removed = router.remove_model("synthetic", None).await.unwrap();
        assert!(!removed.unloaded);
        assert!(!removed.reply.ok);
        assert_eq!(removed.reply.status.to_string(), status[..3]);
        assert_eq!(removed.reply.body.unwrap()["error"]["code"], "model_in_use");
        let calls = up.seen();
        assert_eq!(
            calls.iter().map(|r| r.method.as_str()).collect::<Vec<_>>(),
            ["POST", "DELETE", "POST", "DELETE"]
        );
        assert!(calls
            .iter()
            .all(|r| r.header("x-noevia-maintenance").is_none()));
    }
}

#[tokio::test]
async fn either_unload_success_is_remembered_and_success_never_retries() {
    for first_ok in [false, true] {
        let up = upstream().await;
        let unloads = Arc::new(AtomicUsize::new(0));
        let deletes = Arc::new(AtomicUsize::new(0));
        up.set(move |r| {
            if r.method == "POST" {
                let first = unloads.fetch_add(1, Ordering::SeqCst) == 0;
                reply(
                    if first == first_ok {
                        "200 OK"
                    } else {
                        "404 Not Found"
                    },
                    "{}",
                )
            } else {
                reply(
                    if deletes.fetch_add(1, Ordering::SeqCst) == 0 {
                        "409 Conflict"
                    } else {
                        "200 OK"
                    },
                    "{}",
                )
            }
        });
        let removed = Router::new(&up.base(), None)
            .unwrap()
            .remove_model("synthetic", None)
            .await
            .unwrap();
        assert!(removed.reply.ok);
        assert!(removed.unloaded);
        assert_eq!(up.seen().len(), 4);
    }
    let up = upstream().await;
    up.set(|_| reply("200 OK", "{}"));
    let removed = Router::new(&up.base(), None)
        .unwrap()
        .remove_model("synthetic", None)
        .await
        .unwrap();
    assert!(removed.unloaded);
    assert_eq!(up.seen().len(), 2);
}

#[tokio::test]
async fn unload_transport_failure_is_tolerated_but_delete_transport_failure_is_not_retried() {
    for fail_delete in [false, true] {
        let up = upstream().await;
        up.set(move |r| {
            if (r.method == "DELETE") == fail_delete {
                // A truncated HTTP response makes the transport reject, without a real engine.
                (
                    std::time::Duration::ZERO,
                    "HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{".into(),
                )
            } else {
                reply("200 OK", "{}")
            }
        });
        let removed = Router::new(&up.base(), None)
            .unwrap()
            .remove_model("synthetic", None)
            .await;
        if fail_delete {
            assert!(removed.is_err());
        } else {
            let removed = removed.unwrap();
            assert!(removed.reply.ok);
            assert!(!removed.unloaded);
        }
        assert_eq!(up.seen().len(), 2);
    }
}

#[tokio::test]
async fn core_manager_oracle_covers_unloads_retries_and_transport_failures() {
    let cases: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/model-delete-manager.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let up = upstream().await;
        let steps = case["steps"].as_array().unwrap().clone();
        let index = Arc::new(AtomicUsize::new(0));
        let used = Arc::clone(&index);
        up.set(move |r| {
            let step = &steps[used.fetch_add(1, Ordering::SeqCst)];
            assert_eq!(r.method, step["method"].as_str().unwrap());
            if step["transportError"] == true {
                return (
                    std::time::Duration::ZERO,
                    "HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{".into(),
                );
            }
            reply(
                &format!("{} Synthetic", step["status"]),
                &step["body"].to_string(),
            )
        });
        let result = Router::new(&up.base(), None)
            .unwrap()
            .remove_model("folder/é +?#&=!.gguf", None)
            .await;
        let actual = match result {
            Err(_) => json!({"error":true}),
            Ok(r) => {
                json!({"ok":r.reply.ok,"status":r.reply.status,"body":r.reply.body,"unloaded":r.unloaded})
            }
        };
        assert_eq!(actual, case["expected"], "{}", case["name"]);
        assert_eq!(
            index.load(Ordering::SeqCst),
            case["steps"].as_array().unwrap().len()
        );
    }
}

#[tokio::test]
async fn a_redirect_is_a_transport_failure_and_never_follows_or_retries_the_delete() {
    let up = upstream().await;
    up.set(|r| {
        if r.method == "DELETE" {
            reply_with("302 Found", "Location: /must-not-follow\r\n", "{}")
        } else {
            reply("200 OK", "{}")
        }
    });
    assert!(Router::new(&up.base(), None)
        .unwrap()
        .remove_model("synthetic", None)
        .await
        .is_err());
    assert_eq!(up.seen().len(), 2);
    assert!(up.seen().iter().all(|r| r.path != "/must-not-follow"));
}

#[tokio::test]
async fn a_302_without_location_is_rejected_but_304_is_a_settled_refusal() {
    for (status, rejects) in [("302 Found", true), ("304 Not Modified", false)] {
        let up = upstream().await;
        up.set(move |r| {
            if r.method == "DELETE" {
                reply(status, "")
            } else {
                reply("200 OK", "{}")
            }
        });
        let result = Router::new(&up.base(), None)
            .unwrap()
            .remove_model("synthetic", None)
            .await;
        if rejects {
            assert!(result.is_err());
        } else {
            let result = result.unwrap();
            assert!(!result.reply.ok);
            assert!(result.unloaded);
            assert_eq!(result.reply.status, 304);
        }
        assert_eq!(up.seen().len(), if rejects { 2 } else { 4 });
    }
}
